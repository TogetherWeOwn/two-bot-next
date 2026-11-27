"""Synthetic native PRE handback evidence with no provider transport."""

from dataclasses import FrozenInstanceError, replace
from pathlib import Path
import sys
import unittest
from unittest.mock import patch
from uuid import UUID

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[1]), *sys.path]):
    from pre_admission import AdmissionRecord, SettlementEvidence, SettlementSource
    from pre_cleanup_run import WORKFLOW
    from pre_native_handback import (HandbackEvidence, HandbackReceipt, PredecessorPin,
                                     PredecessorReadback, verify_native_pre_handback)
    from pre_target import Target, TargetError


TARGET = Target("pre-fixture-native", 42, 1_800_000_000, "0473/0465")
INITIAL = UUID(int=1)
PREDECESSOR = PredecessorPin(UUID(int=2), "sha256:" + "ab" * 32)
READBACK = PredecessorReadback(PREDECESSOR.worker_version, PREDECESSOR.image_digest, True)
HEAD = "a" * 40
WORKFLOW_ID = 17
WRONG_TARGETS = (
    replace(TARGET, scope="pre-fixture-other"), replace(TARGET, run_id=43),
    replace(TARGET, started_at=TARGET.started_at + 1), replace(TARGET, shape="0475/0465"),
)
BAD_UUIDS = (None, str(PREDECESSOR.worker_version), 2, UUID(int=0))
BAD_DIGESTS = (None, "", "sha256:" + "0" * 64, "sha256:" + "a" * 63,
               "sha256:" + "AB" * 32, "sha256:" + "gg" * 32, PREDECESSOR.image_digest + "\n")
NOT_TRUE = (False, None, 1, "true", [])


class TargetSubclass(Target):
    pass


class PretendSuccess:
    def __eq__(self, other):
        return other == "success"


def fixture(target=TARGET, *, source=SettlementSource.RUNTIME, start_id=UUID(int=3)):
    record = AdmissionRecord(target, start_id, True, source)
    repository = {"id": 1396224242, "full_name": "TogetherWeOwn/two-bot-next"}
    return {
        "record": record,
        "settlement": SettlementEvidence(replace(record), source, True, True),
        "cleanup_run": {
            "id": target.run_id, "workflow_id": WORKFLOW_ID, "run_attempt": 1, "path": WORKFLOW,
            "head_sha": HEAD, "head_branch": "main", "event": "workflow_dispatch",
            "repository": repository, "head_repository": repository.copy(),
            "status": "completed", "conclusion": "success",
        },
        "expected_workflow_id": WORKFLOW_ID,
        "expected_head_sha": HEAD,
        "initial_version": INITIAL,
        "predecessor_pin": PREDECESSOR,
        "evidence": HandbackEvidence(target, READBACK, True, True, True),
    }


class NativePreHandbackTests(unittest.TestCase):
    def refuse(self, code, *, target=TARGET, **changes):
        with self.assertRaisesRegex(TargetError, f"^{code}$"):
            verify_native_pre_handback(target, **(fixture() | changes))

    def test_receipt_is_frozen_canonical_and_offline(self):
        inputs = fixture()
        run = {"display_title": "Ignored API metadata", **inputs["cleanup_run"]}
        changed_run = dict(reversed(list(run.items())), path=f"{WORKFLOW}@refs/heads/main")
        with patch("socket.socket", side_effect=AssertionError("network effect")), \
                patch("subprocess.Popen", side_effect=AssertionError("subprocess effect")):
            receipt = verify_native_pre_handback(TARGET, **inputs)
            self.assertEqual(receipt, verify_native_pre_handback(TARGET, **(inputs | {"cleanup_run": changed_run})))
        self.assertEqual(receipt, HandbackReceipt(TARGET, INITIAL, PREDECESSOR, UUID(int=3),
                                                 SettlementSource.RUNTIME, WORKFLOW_ID, HEAD,
                                                 "ece65425c62c4da680fab7857c4f054f19facc417b0172157cef97d2e5366c66"))
        for item, field in ((receipt, "handback_digest"), (PREDECESSOR, "image_digest"),
                            (READBACK, "independent"), (inputs["evidence"], "exact_actor_drained")):
            with self.subTest(field=field), self.assertRaises(FrozenInstanceError):
                setattr(item, field, None)

    def test_digest_binds_reservation_and_predecessor(self):
        original = verify_native_pre_handback(TARGET, **fixture())
        for target in WRONG_TARGETS:
            with self.subTest(target=target):
                self.assertNotEqual(original.handback_digest,
                                    verify_native_pre_handback(target, **fixture(target)).handback_digest)
        new_digest = "sha256:" + "cd" * 32
        inputs = fixture()
        inputs["predecessor_pin"] = replace(PREDECESSOR, image_digest=new_digest)
        inputs["evidence"] = replace(inputs["evidence"], predecessor=replace(READBACK, image_digest=new_digest))
        self.assertNotEqual(original.handback_digest, verify_native_pre_handback(TARGET, **inputs).handback_digest)

    def test_provider_settlement_can_prove_close_before_start(self):
        receipt = verify_native_pre_handback(TARGET, **fixture(source=SettlementSource.PROVIDER, start_id=None))
        self.assertIsNone(receipt.start_id)
        self.assertIs(receipt.settlement_source, SettlementSource.PROVIDER)

    def test_target_must_be_exact_type(self):
        subclass = TargetSubclass(TARGET.scope, TARGET.run_id, TARGET.started_at, TARGET.shape)
        for target in (None, TARGET.worker_name, subclass):
            with self.subTest(target=target):
                self.refuse("invalid_pre_native_handback_target", target=target)

    def test_admission_record_must_be_durably_closed_for_exact_target(self):
        record = fixture()["record"]
        self.refuse("pre_native_handback_admission_unconfirmed", record=None)
        for field, values in (("target", WRONG_TARGETS), ("closed", NOT_TRUE),
                              ("settled_by", (None, "runtime")), ("start_id", BAD_UUIDS[1:])):
            for value in values:
                with self.subTest(field=field, value=value):
                    self.refuse("pre_native_handback_admission_unconfirmed", record=replace(record, **{field: value}))

    def test_settlement_must_match_exact_closed_record(self):
        settlement = fixture()["settlement"]
        record = settlement.closed
        for wrong in (None, replace(record, closed=1), replace(record, start_id=None),
                      replace(record, target=WRONG_TARGETS[0]), replace(record, settled_by=SettlementSource.PROVIDER)):
            with self.subTest(closed=wrong):
                self.refuse("pre_native_handback_settlement_mismatch", settlement=replace(settlement, closed=wrong))
        self.refuse("pre_native_handback_settlement_mismatch", settlement=None)

    def test_settlement_must_confirm_source_drain_and_fence(self):
        settlement = fixture()["settlement"]
        for field, values, code in (
            ("source", (None, "runtime", SettlementSource.PROVIDER), "settlement_source_mismatch"),
            ("startup_drained", NOT_TRUE, "startup_not_drained"),
            ("restart_fenced", NOT_TRUE, "restart_not_fenced"),
        ):
            for value in values:
                with self.subTest(field=field, value=value):
                    self.refuse(f"pre_native_handback_{code}", settlement=replace(settlement, **{field: value}))

    def test_cleanup_needs_exact_terminal_success(self):
        run = fixture()["cleanup_run"]
        for wrong, status in (
            (dict(run, status="in_progress", conclusion=None), "active"),
            (dict(run, conclusion="failure"), "terminal_unsuccessful"),
            (dict(run, conclusion=None), "unresolved"), (None, "unresolved"),
            (dict(run, conclusion=PretendSuccess()), "unresolved"),
        ):
            with self.subTest(status=status):
                self.refuse(f"pre_native_handback_cleanup_{status}", cleanup_run=wrong)

    def test_cleanup_rejects_wrong_identity_retry_or_pins(self):
        run = fixture()["cleanup_run"]
        for field, value in (
            ("id", TARGET.run_id + 1), ("workflow_id", WORKFLOW_ID + 1),
            ("path", ".github/workflows/staging-pre-reaper.yml"), ("event", "schedule"),
            ("head_branch", "feature"), ("head_sha", "b" * 40), ("run_attempt", 2), ("run_attempt", True),
            ("repository", dict(run["repository"], id=1)),
            ("head_repository", dict(run["head_repository"], full_name="other/two-bot-next")),
        ):
            with self.subTest(field=field, value=value):
                self.refuse("pre_native_handback_cleanup_unresolved", cleanup_run=dict(run, **{field: value}))
        self.refuse("pre_native_handback_cleanup_unresolved", expected_workflow_id=True)
        self.refuse("pre_native_handback_cleanup_unresolved", expected_head_sha=None)

    def test_version_and_pin_must_be_concrete_nonzero_and_distinct(self):
        for version in BAD_UUIDS:
            with self.subTest(version=version):
                self.refuse("invalid_pre_native_handback_initial_version", initial_version=version)
        self.refuse("pre_native_handback_predecessor_is_disposable", initial_version=PREDECESSOR.worker_version)
        self.refuse("invalid_pre_native_handback_predecessor_pin", predecessor_pin=READBACK)
        evidence = fixture()["evidence"]
        for field, values in (("worker_version", BAD_UUIDS), ("image_digest", BAD_DIGESTS)):
            for value in values:
                with self.subTest(field=field, value=value):
                    self.refuse("invalid_pre_native_handback_predecessor_pin",
                                predecessor_pin=replace(PREDECESSOR, **{field: value}))
                    self.refuse("pre_native_handback_predecessor_unconfirmed",
                                evidence=replace(evidence, predecessor=replace(READBACK, **{field: value})))

    def test_readback_must_independently_match_both_pin_fields(self):
        evidence = fixture()["evidence"]
        for readback in (PREDECESSOR, *(replace(READBACK, independent=value) for value in NOT_TRUE)):
            with self.subTest(readback=readback):
                self.refuse("pre_native_handback_predecessor_unconfirmed", evidence=replace(evidence, predecessor=readback))
        for readback in (replace(READBACK, worker_version=INITIAL), replace(READBACK, image_digest="sha256:" + "c" * 64)):
            with self.subTest(readback=readback):
                self.refuse("pre_native_handback_predecessor_changed", evidence=replace(evidence, predecessor=readback))

    def test_handback_evidence_must_match_exact_target(self):
        evidence = fixture()["evidence"]
        self.refuse("wrong_pre_native_handback_evidence_target", evidence=None)
        for target in WRONG_TARGETS:
            with self.subTest(target=target):
                self.refuse("wrong_pre_native_handback_evidence_target", evidence=replace(evidence, target=target))

    def test_promotion_inhibit_and_exact_actor_drain_must_be_affirmative(self):
        evidence = fixture()["evidence"]
        for field, code in (("promotion_disabled", "promotion_not_disabled"),
                            ("exact_actor_drained", "actor_not_drained"),
                            ("independent_readback", "actor_readback_not_independent")):
            for value in NOT_TRUE:
                with self.subTest(field=field, value=value):
                    self.refuse(f"pre_native_handback_{code}", evidence=replace(evidence, **{field: value}))


if __name__ == "__main__":
    unittest.main()
