"""SYNTHETIC phase/recovery composition, not full lifecycle or runtime evidence.

The paired fixture compares ordinary authority/state and separate DB recovery
claims. Telemetry, resource isolation, process restoration and authenticated
external enforcement are still incomplete. No checker is mocked or bypassed in
the real archive/transition entry points; runtime admission stays unsupported.
"""

import copy
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from announcement_lifecycle_fixtures import CLEANUP, INTERIOR, READ, SETUP
from announcement_provenance_fixtures import at, evidence_index
from announcement_runtime_authority import check_authority_graph_claims
from announcement_runtime_lifecycle import check_lifecycle_ledger_claims, check_lifecycle_phase_claims
from announcement_runtime_policy import runtime_preflight
from announcement_runtime_recovery import check_recovery_ownership_claims
from announcement_runtime_schema import Refusal
from announcement_runtime_transitions import _check_transition_states, check_transition_claims
from test_announcement_runtime_recovery_boundary import PRIORS
from test_announcement_runtime_recovery_grant import GRANT
from test_announcement_runtime_recovery_ownership import set_claim
from test_announcement_runtime_recovery_states import state_fixture


class RecoveryPhases(unittest.TestCase):
    def reject_both(self, mutate, code):
        for prior in PRIORS:
            with self.subTest(prior=prior):
                receipt, records, now = state_fixture(prior)
                mutate(receipt, records)
                with self.assertRaisesRegex(Refusal, "announcement evidence refused: " + code + "$"):
                    check_lifecycle_phase_claims(receipt, evidence_index(records), now)

    def test_paired_graphs_compare_without_full_receipt_or_runtime_admission(self):
        for prior in PRIORS:
            with self.subTest(prior=prior):
                receipt, records, now = state_fixture(prior)
                self.assertLessEqual(len(records), 256)
                for check in (check_authority_graph_claims, _check_transition_states,
                              check_lifecycle_ledger_claims, check_lifecycle_phase_claims):
                    self.assertIsNone(check(receipt, evidence_index(records), now))
                self.assertNotEqual(records["match-" + CLEANUP]["data"]["execution_ref"],
                                    records["match-" + SETUP]["data"]["execution_ref"])
                with self.assertRaises(Refusal):
                    check_transition_claims(receipt, evidence_index(records), now)
                with self.assertRaisesRegex(Refusal, "unsupported$"):
                    runtime_preflight()
                self.assertFalse(receipt["promotable"])

    def test_each_separate_recovery_layer_remains_mandatory(self):
        for ref in ("runtime-recovery-plan", GRANT, "runtime-recovery-checkpoints",
                    "runtime-recovery-fences", "runtime-recovery-installations",
                    "recovery-install-read-cleanup-controller", "match-" + CLEANUP):
            with self.subTest(ref=ref):
                self.reject_both(lambda r, e: e.pop(ref), "unresolved_reference")

    def test_cleanup_cannot_borrow_an_ordinary_named_checkpoint(self):
        self.reject_both(lambda r, e: set_claim(r, e, authority_ref="checkpoint-3"),
                         "recovery_named_checkpoint")

    def test_ordinary_write_cannot_borrow_a_recovery_checkpoint(self):
        self.reject_both(lambda r, e: e["transition-" + INTERIOR]["data"]["claim"].update(
            authority_ref=r["lifecycle"]["cleanup"][0]["authority_ref"]), "lifecycle_phase_checkpoint")

    def test_cleanup_witness_cannot_copy_ordinary_grant_nonce_or_epoch(self):
        for field in ("execution_ref", "nonce_ref", "authority_epoch"):
            with self.subTest(field=field):
                self.reject_both(lambda r, e: e["match-" + CLEANUP]["data"].update(
                    {field: e["match-" + SETUP]["data"][field]}), "recovery_ownership_match")

    def test_setup_and_setting_phases_cannot_be_replaced_by_business_phase(self):
        for ref in ("checkpoint-1", "checkpoint-3"):
            with self.subTest(ref=ref):
                def mutate(receipt, records):
                    checkpoint = next(c for c in receipt["attempt"]["checkpoints"] if c["ref"] == ref)
                    checkpoint["phase"] = "effect_batch"
                    records[checkpoint["mutation_fence_ref"]]["data"]["phase"] = "effect_batch"
                self.reject_both(mutate, "lifecycle_phase_checkpoint")

    def test_ordinary_runtime_and_installation_reads_keep_sixty_second_boundary(self):
        for prior in PRIORS:
            for ref in ("runtime-read-controller-1", "runtime-read-verifier-1",
                        "install-read-controller-1", "install-read-verifier-1"):
                with self.subTest(prior=prior, ref=ref):
                    receipt, records, now = state_fixture(prior)
                    # This narrow phase check compares age, not history hashes;
                    # complete ordinary authority remains a separate mandatory check.
                    records[ref]["data"]["claim"]["read_at"] = at("00:01:00")
                    self.assertIsNone(check_lifecycle_phase_claims(receipt, evidence_index(records), now))
                    records[ref]["data"]["claim"]["read_at"] = at("00:00:59")
                    self.assertIsNone(check_recovery_ownership_claims(receipt, evidence_index(records), now))
                    with self.assertRaisesRegex(Refusal, "stale_mutation_read$"):
                        check_lifecycle_phase_claims(receipt, evidence_index(records), now)

    def test_recovery_does_not_extend_ordinary_fence_expiry(self):
        self.reject_both(lambda r, e: e["fence-1"]["data"].update(valid_until=at("00:02:00")),
                         "lifecycle_phase_checkpoint")

    def test_cleanup_ledger_does_not_admit_read_or_wire_methods(self):
        for method in ("db_read", "member_get"):
            with self.subTest(method=method):
                def mutate(receipt, records):
                    next(e for e in receipt["observations"]["actual_effects"]
                         if e["operation_id"] == CLEANUP)["method"] = method
                self.reject_both(mutate, "lifecycle_transition")

    def test_cleanup_claim_must_match_complete_transition(self):
        self.reject_both(lambda r, e: r["lifecycle"]["cleanup"][0].update(
            ownership_match_ref="different-witness"), "lifecycle_transition")

    def test_missing_cleanup_transition_is_not_inferred_from_ledger(self):
        self.reject_both(lambda r, e: e.pop("transition-" + CLEANUP), "unresolved_reference")

    def test_cleanup_time_must_be_between_observation_end_and_restoration(self):
        for clock in ("00:19:59", "00:21:01"):
            with self.subTest(clock=clock):
                def mutate(receipt, records):
                    set_claim(receipt, records, observed_at=at(clock))
                    next(e for e in receipt["observations"]["actual_effects"]
                         if e["operation_id"] == CLEANUP)["observed_at"] = at(clock)
                self.reject_both(mutate, "lifecycle_operation_time")

    def test_empty_cleanup_cannot_bypass_separate_recovery_comparison(self):
        self.reject_both(lambda r, e: r["lifecycle"].update(cleanup=[]), "array_size")

    def test_unlisted_post_observation_read_remains_unassigned(self):
        def mutate(receipt, records):
            next(e for e in receipt["observations"]["actual_effects"]
                 if e["operation_id"] == READ)["observed_at"] = receipt["attempt"]["observation_ended_at"]
        self.reject_both(mutate, "unassigned_lifecycle_effect")

    def test_setup_and_cleanup_cannot_overlap(self):
        self.reject_both(lambda r, e: r["lifecycle"]["cleanup"].append(
            copy.deepcopy(r["lifecycle"]["setup"][0])), "lifecycle_operation")

    def test_duplicate_cleanup_membership_refuses(self):
        self.reject_both(lambda r, e: r["lifecycle"]["cleanup"].append(
            copy.deepcopy(r["lifecycle"]["cleanup"][0])), "lifecycle_ledger_order")

    def test_current_recovery_still_does_not_extend_ordinary_execution_lease(self):
        for prior in PRIORS:
            with self.subTest(prior=prior):
                receipt, records, now = state_fixture(prior)
                receipt["authority"]["execution"]["expires_at"] = at("00:22:00")
                self.assertIsNone(check_recovery_ownership_claims(receipt, evidence_index(records), now))
                with self.assertRaisesRegex(Refusal, "expiry$"):
                    check_authority_graph_claims(receipt, evidence_index(records), now)


if __name__ == "__main__":
    unittest.main()
