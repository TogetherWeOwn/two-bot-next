"""SYNTHETIC public transition regressions; NOT SQL, actual effects or qualification."""

import copy
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from announcement_provenance_fixtures import evidence_index
from announcement_recovery_transition_fixtures import (
    AUDIT_CLEANUP, SETUP, recovery_transition_fixture,
)
from announcement_telemetry_fixtures import binding
from announcement_transition_fixtures import transition_fixture
from announcement_runtime_lifecycle import (
    check_lifecycle_ledger_claims, check_lifecycle_phase_claims,
)
from announcement_runtime_policy import runtime_preflight
from announcement_runtime_recovery import check_recovery_ownership_claims
from announcement_runtime_schema import Refusal
from announcement_runtime_transitions import check_transition_claims
from test_announcement_runtime_recovery_grant import GRANT
from test_announcement_runtime_recovery_reads import CLEAN_CP


class RecoveryTransitions(unittest.TestCase):
    def reject(self, mutate, code):
        receipt, records, now = recovery_transition_fixture()
        self.assertIsNone(check_transition_claims(receipt, evidence_index(records), now))
        mutate(receipt, records)
        with self.assertRaisesRegex(Refusal, "announcement evidence refused: " + code + "$"):
            check_transition_claims(receipt, evidence_index(records), now)

    def test_public_transition_entry_composes_every_layer_but_never_admits_runtime(self):
        receipt, records, now = recovery_transition_fixture()
        self.assertLessEqual(len(records), 256)
        self.assertIsNone(check_transition_claims(receipt, evidence_index(records), now))
        self.assertIsNone(check_lifecycle_ledger_claims(receipt, evidence_index(records), now))
        self.assertIsNone(check_lifecycle_phase_claims(receipt, evidence_index(records), now))
        self.assertFalse(receipt["promotable"])
        with self.assertRaisesRegex(Refusal, "unsupported$"):
            runtime_preflight()

    def test_original_effects_and_transactions_are_retained_with_explicit_audit_cleanup(self):
        receipt, records, now = recovery_transition_fixture()
        original, old_records, _ = transition_fixture()
        old_effects = original["observations"]["actual_effects"]
        effects = receipt["observations"]["actual_effects"]
        self.assertEqual({effect["operation_id"] for effect in effects},
                         {effect["operation_id"] for effect in old_effects} | {AUDIT_CLEANUP})
        self.assertEqual(len(effects), len(old_effects) + 1)
        transaction = records["transaction-rsvp-audit"]["data"]
        original_transaction = old_records["transaction-rsvp-audit"]["data"]
        for field in ("transaction_id", "observer_ref", "transition_refs", "committed_at", "outcome"):
            self.assertEqual(transaction[field], original_transaction[field])
        members = [bound["operation_id"] for bound in transaction["effects"]]
        self.assertEqual(members, [bound["operation_id"] for bound in original_transaction["effects"]])
        self.assertEqual(set(members), {"budget-rsvp-db_insert-0", "budget-rsvp-audit-db_insert-0"})
        current_effects = {effect["operation_id"]: effect for effect in effects}
        self.assertEqual(transaction["effects"], [binding(receipt, current_effects[op]) for op in members])
        old_refs = original["observations"]["coverage"][0]["transaction_receipt_refs"]
        self.assertTrue(set(old_refs) < set(receipt["observations"]["coverage"][0]["transaction_receipt_refs"]))
        self.assertEqual(len(receipt["lifecycle"]["setup"]), 1)
        self.assertEqual(receipt["lifecycle"]["setup"][0]["operation_id"], SETUP)
        cleanup = receipt["lifecycle"]["cleanup"]
        self.assertEqual(len(cleanup), 5)
        self.assertEqual({claim["resource_ref"] for claim in cleanup},
                         {"feed", "delivery", "setting", "rsvp", "rsvp-audit"})
        self.assertIsNone(check_transition_claims(receipt, evidence_index(records), now))

    def test_provenance_is_still_mandatory_with_coherent_recovery(self):
        receipt, records, now = recovery_transition_fixture()
        self.assertIsNone(check_recovery_ownership_claims(receipt, evidence_index(records), now))
        del records["external-served-two"]
        self.assertIsNone(check_recovery_ownership_claims(receipt, evidence_index(records), now))
        with self.assertRaisesRegex(Refusal, "unresolved_reference$"):
            check_transition_claims(receipt, evidence_index(records), now)

    def test_resource_isolation_is_still_mandatory_with_coherent_recovery(self):
        receipt, records, now = recovery_transition_fixture()
        del records["isolation-setting"]
        self.assertIsNone(check_recovery_ownership_claims(receipt, evidence_index(records), now))
        with self.assertRaisesRegex(Refusal, "unresolved_reference$"):
            check_transition_claims(receipt, evidence_index(records), now)

    def test_wire_coverage_is_still_mandatory_with_coherent_recovery(self):
        receipt, records, now = recovery_transition_fixture()
        ref = receipt["observations"]["coverage"][0]["wire_receipt_refs"][0]
        del records[ref]
        self.assertIsNone(check_recovery_ownership_claims(receipt, evidence_index(records), now))
        with self.assertRaisesRegex(Refusal, "unresolved_reference$"):
            check_transition_claims(receipt, evidence_index(records), now)

    def test_every_separate_recovery_layer_is_required_by_public_entry(self):
        for ref in (GRANT, "runtime-recovery-checkpoints", "runtime-recovery-fences",
                    "runtime-recovery-installations", "recovery-install-read-cleanup-controller"):
            with self.subTest(ref=ref):
                self.reject(lambda r, e: e.pop(ref), "unresolved_reference")

    def test_each_cleanup_witness_is_required_by_public_entry(self):
        receipt, _, _ = recovery_transition_fixture()
        for claim in receipt["lifecycle"]["cleanup"]:
            with self.subTest(operation=claim["operation_id"]):
                self.reject(lambda r, e: e.pop(claim["ownership_match_ref"]), "unresolved_reference")

    def test_cleanup_witness_cannot_substitute_ordinary_execution_scope(self):
        for field in ("execution_ref", "nonce_ref", "authority_epoch"):
            with self.subTest(field=field):
                def mutate(receipt, records):
                    claim = receipt["lifecycle"]["cleanup"][0]
                    records[claim["ownership_match_ref"]]["data"][field] = records[
                        "match-" + SETUP]["data"][field]
                self.reject(mutate, "recovery_ownership_match")

    def test_cleanup_cannot_borrow_an_ordinary_checkpoint(self):
        def mutate(receipt, records):
            claim = receipt["lifecycle"]["cleanup"][0]
            claim["authority_ref"] = "checkpoint-4"
            records["transition-" + claim["operation_id"]]["data"]["claim"] = copy.deepcopy(claim)
        self.reject(mutate, "recovery_named_checkpoint")

    def test_ordinary_witness_remains_required_beside_complete_recovery(self):
        self.reject(lambda r, e: e.pop("match-" + SETUP), "unresolved_reference")

    def test_separate_recovery_does_not_extend_ordinary_lease(self):
        def mutate(receipt, records):
            receipt["authority"]["execution"]["expires_at"] = receipt["attempt"]["ended_at"]
        self.reject(mutate, "expiry")

    def test_recovery_does_not_replace_atomic_rsvp_audit_membership(self):
        def mutate(receipt, records):
            transaction = records["transaction-rsvp-audit"]["data"]
            transaction["effects"] = [bound for bound in transaction["effects"]
                                     if "rsvp-audit" not in bound["operation_id"]]
        self.reject(mutate, "rsvp_atomic_audit")

    def test_cleanup_names_only_the_separate_checkpoint(self):
        receipt, records, now = recovery_transition_fixture()
        self.assertEqual({claim["authority_ref"] for claim in receipt["lifecycle"]["cleanup"]}, {CLEAN_CP})
        self.assertNotIn(CLEAN_CP, {checkpoint["ref"] for checkpoint in receipt["attempt"]["checkpoints"]})
        self.assertEqual(receipt["observations"]["actual_effects"], receipt["observations"]["expected_effects"])
        self.assertEqual([effect["sequence"] for effect in receipt["observations"]["actual_effects"]],
                         list(range(1, len(receipt["observations"]["actual_effects"]) + 1)))
        self.assertIsNone(check_transition_claims(receipt, evidence_index(records), now))


if __name__ == "__main__":
    unittest.main()
