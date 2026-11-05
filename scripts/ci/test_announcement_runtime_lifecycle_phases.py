"""Partial fabricated phase claims, NEVER a successful lifecycle or authority.

The authority and lifecycle fixtures are deliberately spliced, not reconciled
into an authenticated or fully consistent packet. Only the new narrow phase
comparison runs; upstream comparisons are not mocked or claimed to pass.
Every baseline ends in the explicit unsupported recovery refusal. No scopes,
SQL, ownership, stored values, effect totals or real phase completion are proved.
"""

import copy
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from announcement_authority_fixtures import authority_fixture
from announcement_lifecycle_fixtures import CLEANUP, INTERIOR, READ, SETUP, lifecycle_fixture
from announcement_provenance_fixtures import at, evidence_index
from announcement_setting_fixtures import add_checkpoint
from announcement_runtime_lifecycle import check_lifecycle_phase_claims
from announcement_runtime_policy import runtime_preflight
from announcement_runtime_schema import Refusal


def fixture():
    receipt, records, _ = authority_fixture()
    partial, ledger, now = lifecycle_fixture()
    for field in ("resources", "lifecycle", "observations"):
        receipt[field] = partial[field]
    for field in ("started_at", "ended_at", "observation_started_at", "observation_ended_at"):
        receipt["attempt"][field] = partial["attempt"][field]
    receipt["rollout"].update(started_at=partial["rollout"]["started_at"],
                              ended_at=partial["rollout"]["ended_at"])
    records.update(ledger)
    for operation, clock, phase in ((READ, "00:12:00", "effect_batch"),
                                     (INTERIOR, "00:15:00", "transition"),
                                     (CLEANUP, "00:20:30", "effect_batch")):
        ref = add_checkpoint(receipt, records, clock)
        checkpoint = next(x for x in receipt["attempt"]["checkpoints"] if x["ref"] == ref)
        checkpoint["phase"] = phase
        records[checkpoint["mutation_fence_ref"]]["data"]["phase"] = phase
        if operation != READ:
            bind(receipt, records, operation, ref)
    bind(receipt, records, SETUP, "checkpoint-1")
    return receipt, records, now


def bind(receipt, records, operation, ref):
    records["transition-" + operation]["data"]["claim"]["authority_ref"] = ref
    for field in ("setup", "cleanup"):
        for claim in receipt["lifecycle"][field]:
            if claim["operation_id"] == operation:
                claim["authority_ref"] = ref


def checkpoint_for(receipt, records, operation):
    ref = records["transition-" + operation]["data"]["claim"]["authority_ref"]
    return next(x for x in receipt["attempt"]["checkpoints"] if x["ref"] == ref)


def rephase(receipt, records, operation, phase):
    checkpoint = checkpoint_for(receipt, records, operation)
    checkpoint["phase"] = phase
    records[checkpoint["mutation_fence_ref"]]["data"]["phase"] = phase


class LifecyclePhases(unittest.TestCase):
    def reject(self, mutate=None, code="unsupported_recovery_authority"):
        receipt, records, now = fixture()
        if mutate:
            mutate(receipt, records)
        with self.assertRaisesRegex(Refusal, "announcement evidence refused: " + code + "$"):
            check_lifecycle_phase_claims(receipt, evidence_index(records), now)

    def test_coherent_early_phases_still_refuse_unimplemented_recovery(self):
        self.reject()
        with self.assertRaisesRegex(Refusal, "unsupported"):
            runtime_preflight()

    def test_setup_cannot_borrow_a_coherent_effect_batch_phase(self):
        self.reject(lambda r, s: rephase(r, s, SETUP, "effect_batch"),
                    "lifecycle_phase_checkpoint")

    def test_setting_transition_cannot_borrow_a_coherent_effect_batch_phase(self):
        self.reject(lambda r, s: rephase(r, s, INTERIOR, "effect_batch"),
                    "lifecycle_phase_checkpoint")

    def test_business_write_requires_effect_batch_not_setting_transition(self):
        def mutate(receipt, records):
            receipt["resources"]["entries"][0]["kind"] = "feed"
            self.assertEqual(checkpoint_for(receipt, records, INTERIOR)["phase"], "transition")
        self.reject(mutate, "lifecycle_phase_checkpoint")
        self.reject(lambda r, s: (r["resources"]["entries"][0].update(kind="feed"),
                                 rephase(r, s, INTERIOR, "effect_batch")))

    def test_setup_cannot_be_omitted_from_ledger(self):
        def mutate(receipt, records):
            receipt["lifecycle"]["setup"] = [copy.deepcopy(receipt["lifecycle"]["cleanup"][0])]
            receipt["lifecycle"]["setup"][0]["operation_id"] = INTERIOR
        self.reject(mutate, "unassigned_lifecycle_effect")

    def test_setup_claim_cannot_disagree_with_transition(self):
        self.reject(lambda r, s: r["lifecycle"]["setup"][0].update(authority_ref="other"),
                    "lifecycle_transition")

    def test_one_write_cannot_be_both_setup_and_cleanup(self):
        self.reject(lambda r, s: r["lifecycle"]["cleanup"].append(
            copy.deepcopy(r["lifecycle"]["setup"][0])), "lifecycle_operation")

    def test_unknown_ledger_operation_refuses(self):
        self.reject(lambda r, s: r["lifecycle"]["setup"][0].update(operation_id="missing"),
                    "lifecycle_operation")

    def test_mismatched_fence_phase_or_checkpoint_refuses(self):
        for change in ({"phase": "effect_batch"}, {"checkpoint_ref": "other"},
                       {"observed_at": at("00:01:59")}):
            with self.subTest(change=change):
                self.reject(lambda r, s: s["fence-1"]["data"].update(change), "lifecycle_fence")

    def test_named_checkpoint_cannot_be_later_than_write(self):
        def mutate(receipt, records):
            checkpoint = checkpoint_for(receipt, records, SETUP)
            checkpoint["checked_at"] = at("00:02:01")
            records[checkpoint["mutation_fence_ref"]]["data"]["observed_at"] = at("00:02:01")
        self.reject(mutate, "lifecycle_phase_checkpoint")

    def test_named_checkpoint_expiry_is_exclusive(self):
        self.reject(lambda r, s: s["fence-1"]["data"].update(valid_until=at("00:02:00")),
                    "lifecycle_phase_checkpoint")

    def test_each_named_setup_read_must_be_current_at_the_write(self):
        for prefix in ("runtime", "install"):
            for role in ("controller", "verifier"):
                ref = prefix + "-read-" + role + "-1"
                with self.subTest(ref=ref):
                    self.reject(lambda r, s: s[ref]["data"]["claim"].update(
                        read_at=at("00:00:59")), "stale_mutation_read")
                    self.reject(lambda r, s: s[ref]["data"]["claim"].update(
                        read_at=at("00:01:00")))

    def test_a_fresh_other_checkpoint_cannot_repair_named_stale_setup(self):
        def mutate(receipt, records):
            ref = add_checkpoint(receipt, records, "00:02:00")
            other = next(x for x in receipt["attempt"]["checkpoints"] if x["ref"] == ref)
            other["phase"] = "setup"
            records[other["mutation_fence_ref"]]["data"]["phase"] = "setup"
            records["runtime-read-controller-1"]["data"]["claim"]["read_at"] = at("00:00:59")
        self.reject(mutate, "stale_mutation_read")

    def test_an_unlisted_post_observation_read_has_no_default_effect_phase(self):
        def mutate(receipt, records):
            read = next(x for x in receipt["observations"]["actual_effects"] if x["operation_id"] == READ)
            read["observed_at"] = receipt["attempt"]["observation_ended_at"]
        self.reject(mutate, "unassigned_lifecycle_effect")

    def test_cleanup_cannot_use_normal_fresh_setup_or_effect_or_final_authority(self):
        for phase in ("setup", "transition", "effect_batch", "final"):
            with self.subTest(phase=phase):
                self.reject(lambda r, s: rephase(r, s, CLEANUP, phase))

    def test_installation_inventory_must_match_checkpoint_count(self):
        self.reject(lambda r, s: r["authority"]["installation_approval_refs"].pop(),
                    "installation_approval")


if __name__ == "__main__":
    unittest.main()
