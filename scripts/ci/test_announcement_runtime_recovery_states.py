"""SYNTHETIC partial authority/state composition, NOT a complete receipt or CAS.

Add the two ordinary witnesses missing from paired_fixture(). The private state
comparison is exercised without mocking any checker. It still runs the complete
ordinary and separate recovery authority graphs. The PUBLIC transition entry
point retains its mandatory telemetry/resource checks and refuses this partial
fixture; the narrower phase comparison grants no runtime capability. Full-window telemetry,
literal resource ownership and process restoration are NOT demonstrated here.
"""

from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from announcement_lifecycle_fixtures import CLEANUP, INTERIOR, SETUP
from announcement_provenance_fixtures import envelope, evidence_index
from announcement_runtime_authority import check_authority_graph_claims
from announcement_runtime_evidence import canonical_digest
from announcement_runtime_lifecycle import check_lifecycle_phase_claims
from announcement_runtime_policy import runtime_preflight
from announcement_runtime_recovery import check_recovery_ownership_claims
from announcement_runtime_schema import Refusal
from announcement_runtime_transitions import _check_transition_states, check_transition_claims
from test_announcement_runtime_recovery_boundary import PRIORS, paired_fixture
from test_announcement_runtime_recovery_grant import GRANT


def state_fixture(prior="absent"):
    receipt, records, now = paired_fixture(prior)
    resource = receipt["resources"]["entries"][0]
    authority = receipt["authority"]
    for operation in (SETUP, INTERIOR):
        effect = next(effect for effect in receipt["observations"]["actual_effects"]
                      if effect["operation_id"] == operation)
        claim = records["transition-" + operation]["data"]["claim"]
        ref = claim["ownership_match_ref"]
        records[ref] = envelope(ref, "ownership_match", {
            "operation_id": operation, "resource_ref": resource["ref"],
            "identity_sha256": canonical_digest(resource["identity"]),
            "key_sha256": resource["identity"]["key_sha256"], "owner_ref": resource["owner"],
            "execution_ref": authority["execution"]["ref"],
            "nonce_ref": authority["execution"]["nonce_ref"], "authority_epoch": authority["epoch"],
            "resource_epoch": receipt["attempt"]["enforcement_epoch"],
            "checkpoint_ref": claim["authority_ref"], "method": effect["method"],
            "before_state_sha256": canonical_digest(records[claim["before_state_ref"]]["data"]),
            "after_state_sha256": canonical_digest(records[claim["actual_after_state_ref"]]["data"]),
            "observer_ref": receipt["observations"]["coverage"][0]["observer_ref"],
            "observed_at": effect["observed_at"], "boundary": "external_compare_and_match",
            "outcome": "matched"})
    return receipt, records, now


class RecoveryStates(unittest.TestCase):
    def reject_both(self, mutate, code):
        for prior in PRIORS:
            with self.subTest(prior=prior):
                receipt, records, now = state_fixture(prior)
                mutate(receipt, records)
                with self.assertRaisesRegex(Refusal, "announcement evidence refused: " + code + "$"):
                    _check_transition_states(receipt, evidence_index(records), now)

    def test_paired_states_and_phases_compare_but_full_entry_and_runtime_still_refuse(self):
        for prior in PRIORS:
            with self.subTest(prior=prior):
                receipt, records, now = state_fixture(prior)
                self.assertLessEqual(len(records), 256)
                self.assertIsNone(check_authority_graph_claims(receipt, evidence_index(records), now))
                self.assertIsNone(_check_transition_states(receipt, evidence_index(records), now))
                self.assertNotEqual(records["match-" + CLEANUP]["data"]["execution_ref"],
                                    records["match-" + SETUP]["data"]["execution_ref"])
                with self.assertRaises(Refusal):
                    check_transition_claims(receipt, evidence_index(records), now)
                self.assertIsNone(check_lifecycle_phase_claims(receipt, evidence_index(records), now))
                with self.assertRaisesRegex(Refusal, "unsupported$"):
                    runtime_preflight()

    def test_cleanup_requires_complete_separate_graph_in_state_comparison(self):
        for ref in (GRANT, "runtime-recovery-checkpoints", "runtime-recovery-fences",
                    "runtime-recovery-installations", "match-" + CLEANUP):
            with self.subTest(ref=ref):
                self.reject_both(lambda r, e: e.pop(ref), "unresolved_reference")

    def test_ordinary_writes_still_require_their_own_witnesses(self):
        for operation in (SETUP, INTERIOR):
            with self.subTest(operation=operation):
                self.reject_both(lambda r, e: e.pop("match-" + operation), "unresolved_reference")

    def test_ordinary_witness_cannot_copy_recovery_scope(self):
        for field in ("execution_ref", "nonce_ref", "authority_epoch"):
            with self.subTest(field=field):
                def mutate(receipt, records):
                    records["match-" + INTERIOR]["data"][field] = records[
                        "match-" + CLEANUP]["data"][field]
                self.reject_both(mutate, "ownership_match")

    def test_ordinary_witness_still_binds_complete_state_hashes(self):
        for field in ("before_state_sha256", "after_state_sha256"):
            with self.subTest(field=field):
                self.reject_both(lambda r, e: e["match-" + INTERIOR]["data"].update(
                    {field: "0" * 64}), "ownership_match")

    def test_ordinary_chain_cannot_skip_setup_even_when_recovery_chain_matches(self):
        for prior in PRIORS:
            with self.subTest(prior=prior):
                receipt, records, now = state_fixture(prior)
                records["transition-" + INTERIOR]["data"]["claim"]["before_state_ref"] = "state-before"
                self.assertIsNone(check_recovery_ownership_claims(receipt, evidence_index(records), now))
                with self.assertRaisesRegex(Refusal, "transition_chain$"):
                    _check_transition_states(receipt, evidence_index(records), now)

    def test_ordinary_intermediate_observer_is_not_exempted_by_cleanup(self):
        self.reject_both(lambda r, e: e["state-setup"]["data"].update(
            observer_ref="other-observer"), "transition_state_observer")

    def test_ordinary_after_state_time_remains_exact(self):
        self.reject_both(lambda r, e: e["state-setup"]["data"].update(
            observed_at="2026-01-01T00:02:01Z"), "transition_state_time")

    def test_cleanup_does_not_replace_ordinary_named_checkpoint(self):
        self.reject_both(lambda r, e: e["transition-" + INTERIOR]["data"]["claim"].update(
            authority_ref=r["lifecycle"]["cleanup"][0]["authority_ref"]), "transition_authority")

    def test_recovery_witness_cannot_copy_ordinary_scope(self):
        for field in ("execution_ref", "nonce_ref", "authority_epoch"):
            with self.subTest(field=field):
                def mutate(receipt, records):
                    records["match-" + CLEANUP]["data"][field] = records[
                        "match-" + INTERIOR]["data"][field]
                self.reject_both(mutate, "recovery_ownership_match")


if __name__ == "__main__":
    unittest.main()
