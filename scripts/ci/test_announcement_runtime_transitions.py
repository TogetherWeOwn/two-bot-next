"""Hermetic owned-state transition adversaries; not mutation or restoration proof.

Passing here shows only that fabricated, unauthenticated state snapshots and
ownership witnesses agree with a fabricated effect ledger. The fixture is a
SYNTHETIC single-instant accounting example: it is not lifecycle evidence, not
SQL compare-and-set proof, not restoration proof, and runtime_preflight stays
unsupported. Semantic mutations repin the derived ownership snapshot hashes so a
refusal comes from the predicate under test, not from an incidental stale digest.
"""

import copy
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from announcement_provenance_fixtures import at, evidence_index
from announcement_resource_fixtures import repin_authority, repin_resources
from announcement_transition_fixtures import (
    add_late_effect_checkpoint, repin_ownership, transition_fixture,
)
from announcement_runtime_authority import check_authority_graph_claims
from announcement_runtime_evidence import canonical_digest
from announcement_runtime_policy import runtime_preflight
from announcement_runtime_schema import Refusal
from announcement_runtime_transitions import check_transition_claims

FEED_INS = "budget-feed-db_insert-0"
FEED_DEL = "budget-feed-db_delete_owned-0"
DELIVERY_INS = "budget-delivery-db_insert-0"
DELIVERY_CAS = "budget-delivery-db_cas-0"
DELIVERY_DEL = "budget-delivery-db_delete_owned-0"
SETTING_INS = "budget-setting-db_insert-0"
SETTING_CAS0 = "budget-setting-db_cas-0"
SETTING_CAS1 = "budget-setting-db_cas-1"
SETTING_DEL = "budget-setting-db_delete_owned-0"
RSVP_INS = "budget-rsvp-db_insert-0"
RSVP_DEL = "budget-rsvp-db_delete_owned-0"
AUDIT_INS = "budget-rsvp-audit-db_insert-0"
WRITES = (FEED_INS, FEED_DEL, DELIVERY_INS, DELIVERY_CAS, DELIVERY_DEL, SETTING_INS,
          SETTING_CAS0, SETTING_CAS1, SETTING_DEL, RSVP_INS, RSVP_DEL, AUDIT_INS)
EFFECT_BATCH = "checkpoint-4"
OTHER_PHASES = ("checkpoint-0", "checkpoint-1", "checkpoint-2", "checkpoint-3", "checkpoint-5")


def match(records, operation):
    return records["match-" + operation]["data"]


def state(records, operation):
    return records["state-" + operation]["data"]


def claim(records, operation):
    return records["transition-" + operation]["data"]["claim"]


def set_after(records, operation, ref):
    """Point one transition at a different after state (intended == actual)."""
    claim(records, operation).update(intended_after_state_ref=ref, actual_after_state_ref=ref)


def set_state_claim(records, operation, **changes):
    state(records, operation)["claim"].update(changes)


def replace_state_claim(records, operation, **fields):
    ref = state(records, operation)["claim"]["ref"]
    state(records, operation)["claim"] = {"ref": ref, **fields}


def set_endpoint(receipt, records, ref, **fields):
    """Rewrite a resource endpoint state coherently in claim, plan and pins."""
    records[ref]["data"]["claim"] = {"ref": ref, **fields}
    entry = next(x for x in receipt["resources"]["states"] if x["ref"] == ref)
    entry.clear()
    entry.update(ref=ref, **fields)
    repin_resources(receipt, records)
    repin_authority(receipt, records)


class Transitions(unittest.TestCase):
    def check(self, receipt, records, now):
        return check_transition_claims(receipt, evidence_index(records), now)

    def reject(self, mutate, code=None, *, repin=True):
        receipt, records, now = transition_fixture()
        mutate(receipt, records)
        if repin:
            repin_ownership(receipt, records)
        pattern = "announcement evidence refused" if code is None else (
            "announcement evidence refused: " + code + "$")
        with self.assertRaisesRegex(Refusal, pattern):
            self.check(receipt, records, now)

    def accept(self, mutate):
        receipt, records, now = transition_fixture()
        mutate(receipt, records)
        repin_ownership(receipt, records)
        self.assertIsNone(self.check(receipt, records, now))


class Coherence(Transitions):
    def test_synthetic_chain_uses_every_record_and_is_not_runtime_authority(self):
        receipt, records, now = transition_fixture()
        index = evidence_index(records)
        self.assertIsNone(check_transition_claims(receipt, index, now))
        index.require_all_used()
        self.assertFalse(receipt["promotable"])
        with self.assertRaisesRegex(Refusal, "unsupported"):
            runtime_preflight()

    def test_fixture_is_single_instant_with_one_distinct_witness_per_write(self):
        receipt, records, _ = transition_fixture()
        writes = [x for x in receipt["observations"]["actual_effects"]
                  if x["method"].startswith("db_") and x["method"] != "db_read"]
        self.assertEqual(list(WRITES), [x["operation_id"] for x in writes])
        self.assertEqual({at("00:08:00")}, {x["observed_at"] for x in writes})
        witnesses = [claim(records, x)["ownership_match_ref"] for x in WRITES]
        self.assertEqual(len(WRITES), len(set(witnesses)))
        for effect in writes:
            witness = match(records, effect["operation_id"])
            self.assertEqual(effect["method"], witness["method"])
            self.assertEqual("external_compare_and_match", witness["boundary"])
            self.assertEqual("matched", witness["outcome"])
            self.assertEqual(EFFECT_BATCH, claim(records, effect["operation_id"])["authority_ref"])
        versions = [state(records, x)["claim"]["version"]
                    for x in (SETTING_INS, SETTING_CAS0, SETTING_CAS1)]
        self.assertEqual(["1", "2", "3"], versions)

    def test_repin_is_idempotent_and_touches_only_snapshot_hashes(self):
        receipt, records, _ = transition_fixture()
        before = copy.deepcopy(records)
        repin_ownership(receipt, records)
        self.assertEqual(before, records)
        set_state_claim(records, SETTING_CAS0, value_sha256="e" * 64)
        repin_ownership(receipt, records)
        changed = [ref for ref in records if records[ref] != before[ref]
                   and records[ref]["kind"] == "ownership_match"]
        self.assertEqual(sorted(["match-" + SETTING_CAS0, "match-" + SETTING_CAS1]),
                         sorted(changed))
        for ref in changed:
            old, new = before[ref]["data"], records[ref]["data"]
            self.assertEqual({k: v for k, v in old.items() if not k.endswith("_state_sha256")},
                             {k: v for k, v in new.items() if not k.endswith("_state_sha256")})

    def test_version_ordering_is_numeric_not_lexicographic(self):
        def mutate(receipt, records):
            set_state_claim(records, SETTING_CAS0, version="9", value_sha256="9" * 64)
            set_state_claim(records, SETTING_CAS1, version="10", value_sha256="a" * 64)
        self.accept(mutate)

    def test_changed_intermediate_value_is_accepted_only_when_snapshot_is_repinned(self):
        def mutate(receipt, records):
            set_state_claim(records, SETTING_CAS0, value_sha256="e" * 64)
        self.accept(mutate)
        self.reject(mutate, "ownership_match", repin=False)


class Schema(Transitions):
    def reject(self, mutate, code=None, *, repin=False):
        # Shape/reference adversaries must not recreate missing digest fields or
        # try to repin a missing/wrong-kind witness before bounded intake.
        return super().reject(mutate, code, repin=repin)

    def test_extra_witness_field_is_refused(self):
        self.reject(lambda r, s: match(s, FEED_INS).update(note="x"))

    def test_each_missing_witness_field_is_refused(self):
        _, records, _ = transition_fixture()
        for field in match(records, FEED_INS):
            with self.subTest(field=field):
                self.reject(lambda r, s, f=field: match(s, FEED_INS).pop(f))

    def test_invalid_enum_and_type_values_are_refused(self):
        bad = {"method": "db_read", "boundary": "bogus", "outcome": "bogus",
               "authority_epoch": 7, "observed_at": "not-a-time", "key_sha256": "xyz"}
        for field, value in bad.items():
            with self.subTest(field=field):
                self.reject(lambda r, s, f=field, v=value: match(s, FEED_INS).update({f: v}))

    def test_wrong_kind_reference_is_refused(self):
        self.reject(lambda r, s: claim(s, FEED_INS).update(
            ownership_match_ref="state-" + FEED_INS), "reference_kind")

    def test_missing_witness_record_is_refused(self):
        self.reject(lambda r, s: s.pop("match-" + FEED_INS), "unresolved_reference")

    def test_wrong_attempt_witness_is_refused(self):
        self.reject(lambda r, s: s["match-" + FEED_INS]["run"].update(attempt=2),
                    "evidence_attempt")

    def test_witness_reused_for_another_operation_is_refused(self):
        self.reject(lambda r, s: claim(s, FEED_DEL).update(
            ownership_match_ref="match-" + FEED_INS), "ownership_match")


class States(Transitions):
    def test_state_resource_identity_and_owner_must_bind_the_resource(self):
        changes = {"resource_ref": "setting", "identity_sha256": "0" * 64,
                   "owner_ref": "other-owner"}
        for field, value in changes.items():
            with self.subTest(field=field):
                self.reject(lambda r, s, f=field, v=value: state(s, FEED_INS).update({f: v}),
                            "transition_state_binding")

    def test_state_claim_ref_must_equal_its_record_ref(self):
        self.reject(lambda r, s: set_state_claim(s, FEED_INS, ref="other-state"),
                    "transition_state_binding")

    def test_state_of_another_resource_is_not_accepted(self):
        self.reject(lambda r, s: set_after(s, FEED_INS, "state-" + DELIVERY_INS),
                    "transition_state_binding")

    def test_intermediate_state_needs_the_full_window_observer(self):
        self.reject(lambda r, s: state(s, FEED_INS).update(observer_ref="resource-observer"),
                    "transition_state_observer")

    def test_after_state_time_must_equal_the_effect_time(self):
        for clock in ("00:07:59", "00:08:01"):
            with self.subTest(clock=clock):
                self.reject(lambda r, s, c=clock: state(s, FEED_INS).update(observed_at=at(c)),
                            "transition_state_time")

    def test_chain_cannot_skip_an_intermediate_state(self):
        self.reject(lambda r, s: claim(s, DELIVERY_DEL).update(
            before_state_ref="state-" + DELIVERY_INS), "transition_chain")

    def test_before_state_cannot_reset_to_the_endpoint_baseline(self):
        self.reject(lambda r, s: claim(s, FEED_DEL).update(before_state_ref="before-feed"),
                    "transition_chain")
        self.reject(lambda r, s: claim(s, SETTING_CAS0).update(
            before_state_ref="before-setting"), "transition_chain")

    def test_after_state_cannot_repeat_the_before_state(self):
        self.reject(lambda r, s: set_after(s, SETTING_CAS0, "state-" + SETTING_INS),
                    "transition_state_reuse")

    def test_after_state_cannot_reuse_an_endpoint_state(self):
        self.reject(lambda r, s: set_after(s, SETTING_INS, "after-setting"),
                    "transition_state_reuse")

    def test_insert_must_create_a_present_state(self):
        self.reject(lambda r, s: replace_state_claim(s, FEED_INS, presence="absent"),
                    "transition_insert")

    def test_insert_into_a_present_state_is_refused(self):
        def mutate(receipt, records):
            set_endpoint(receipt, records, "before-feed", presence="present",
                         version="1", value_sha256="a" * 64)
        self.reject(mutate, "transition_insert")

    def test_cas_must_advance_the_version(self):
        for operation in (SETTING_CAS0, SETTING_CAS1):
            with self.subTest(operation=operation):
                self.reject(lambda r, s, o=operation: set_state_claim(
                    s, o, version="1", value_sha256="7" * 64), "transition_cas")

    def test_cas_noop_with_identical_contents_is_refused(self):
        def mutate(receipt, records):
            old = state(records, SETTING_INS)["claim"]
            set_state_claim(records, SETTING_CAS0, version=old["version"],
                            value_sha256=old["value_sha256"])
        self.reject(mutate, "transition_cas")

    def test_cas_cannot_delete_the_state(self):
        self.reject(lambda r, s: replace_state_claim(s, SETTING_CAS0, presence="absent"),
                    "transition_cas")

    def test_delete_must_leave_the_state_absent(self):
        self.reject(lambda r, s: replace_state_claim(
            s, FEED_DEL, presence="present", version="2", value_sha256="2" * 64),
            "transition_delete")


class Authority(Transitions):
    def test_unresolved_authority_ref_is_refused(self):
        for ref in ("missing-checkpoint", "execution-ref", "match-" + FEED_INS):
            with self.subTest(ref=ref):
                self.reject(lambda r, s, x=ref: claim(s, FEED_INS).update(authority_ref=x),
                            "transition_authority")

    def test_authority_ref_must_be_the_effect_batch_phase(self):
        for ref in OTHER_PHASES:
            with self.subTest(ref=ref):
                self.reject(lambda r, s, x=ref: claim(s, FEED_INS).update(authority_ref=x),
                            "transition_authority")

    def test_witness_must_name_the_same_effect_batch_checkpoint(self):
        for ref in OTHER_PHASES:
            with self.subTest(ref=ref):
                self.reject(lambda r, s, x=ref: match(s, FEED_INS).update(checkpoint_ref=x),
                            "ownership_match")

    def test_another_covering_fence_cannot_repair_the_named_checkpoint_time(self):
        receipt, records, now = transition_fixture()
        late = add_late_effect_checkpoint(receipt, records)
        index = evidence_index(records)
        self.assertIsNone(check_transition_claims(receipt, index, now))
        index.require_all_used()
        self.assertIsNone(check_authority_graph_claims(receipt, evidence_index(records), now))
        claim(records, FEED_INS)["authority_ref"] = late
        match(records, FEED_INS)["checkpoint_ref"] = late
        with self.assertRaisesRegex(Refusal, "transition_authority_time$"):
            check_authority_graph_claims(receipt, evidence_index(records), now)
        with self.assertRaisesRegex(Refusal, "transition_authority_time$"):
            self.check(receipt, records, now)

    def test_another_fresh_checkpoint_cannot_repair_named_stale_reads(self):
        for prefix in ("runtime", "install"):
            with self.subTest(prefix=prefix):
                receipt, records, now = transition_fixture()
                extra_ref = add_late_effect_checkpoint(receipt, records)
                attempt, authority = receipt["attempt"], receipt["authority"]
                index = next(i for i, c in enumerate(attempt["checkpoints"])
                             if c["ref"] == extra_ref)
                checkpoint = attempt["checkpoints"].pop(index)
                install_ref = authority["installation_approval_refs"].pop(index)
                # Reuse the complete clone as an EARLIER checkpoint. Its fence
                # covers 08:00, but one family of paired reads is 61 seconds old.
                checkpoint["checked_at"] = at("00:07:59")
                attempt["checkpoints"].insert(index - 1, checkpoint)
                authority["installation_approval_refs"].insert(index - 1, install_ref)
                install = records[install_ref]["data"]
                install["checked_at"] = at("00:07:59")
                records[checkpoint["mutation_fence_ref"]]["data"]["observed_at"] = at("00:07:59")
                for group, record in (("runtime", checkpoint), ("install", install)):
                    clock = "00:06:59" if group == prefix else "00:07:59"
                    for role in ("controller", "verifier"):
                        read = records[record[role + "_read_ref"]]["data"]
                        read["claim"]["read_at"] = at(clock)
                        history = records[read["history_ref"]]["data"]
                        history["ended_at"] = at(clock)
                        read["claim"]["history_sha256"] = canonical_digest(history)
                receipt["control"]["installation"]["authority_receipt_refs"] = copy.deepcopy(
                    authority["installation_approval_refs"])
                records[authority["execution"]["ref"]]["data"]["installation_approval_refs"] = (
                    copy.deepcopy(authority["installation_approval_refs"]))
                records["runtime-authority-scope"]["data"]["control"] = copy.deepcopy(receipt["control"])
                repin_authority(receipt, records)
                # The unchanged 08:00 checkpoint independently covers the whole
                # ledger. Check that control BEFORE naming the stale checkpoint.
                self.assertIsNone(self.check(receipt, records, now))
                self.assertIsNone(check_authority_graph_claims(receipt, evidence_index(records), now))
                claim(records, FEED_INS)["authority_ref"] = extra_ref
                match(records, FEED_INS)["checkpoint_ref"] = extra_ref
                with self.assertRaisesRegex(Refusal, "stale_mutation_read$"):
                    check_authority_graph_claims(receipt, evidence_index(records), now)
                with self.assertRaisesRegex(Refusal, "stale_mutation_read$"):
                    self.check(receipt, records, now)

    def test_authority_graph_binds_supplied_transition_to_the_exact_write(self):
        changes = ({"operation_id": FEED_DEL}, {"resource_ref": "delivery"},
                   {"observed_at": at("00:08:01")})
        for change in changes:
            with self.subTest(change=change):
                receipt, records, now = transition_fixture()
                self.assertIsNone(check_authority_graph_claims(receipt, evidence_index(records), now))
                claim(records, FEED_INS).update(change)
                with self.assertRaisesRegex(Refusal, "mutation_transition_binding$"):
                    check_authority_graph_claims(receipt, evidence_index(records), now)
        receipt, records, now = transition_fixture()
        records["transition-" + FEED_INS]["data"]["method"] = "db_cas"
        with self.assertRaisesRegex(Refusal, "mutation_transition_binding$"):
            check_authority_graph_claims(receipt, evidence_index(records), now)

    def test_authority_graph_does_not_ignore_multiple_or_wrong_typed_transitions(self):
        for refs, code in ((["transition-" + FEED_INS, "transition-" + FEED_DEL],
                            "mutation_transition_inventory"),
                           (["match-" + FEED_INS], "reference_kind")):
            with self.subTest(refs=refs):
                receipt, records, now = transition_fixture()
                effect = next(x for x in receipt["observations"]["actual_effects"]
                              if x["operation_id"] == FEED_INS)
                effect["db_transition_refs"] = refs
                with self.assertRaisesRegex(Refusal, code + "$"):
                    check_authority_graph_claims(receipt, evidence_index(records), now)

    def test_authority_graph_cannot_borrow_for_an_unknown_or_wrong_phase_checkpoint(self):
        for ref in ("missing-checkpoint", "execution-ref", *OTHER_PHASES):
            with self.subTest(ref=ref):
                receipt, records, now = transition_fixture()
                claim(records, FEED_INS)["authority_ref"] = ref
                with self.assertRaisesRegex(Refusal, "transition_authority$"):
                    check_authority_graph_claims(receipt, evidence_index(records), now)

    def test_every_write_needs_the_effect_batch_authority(self):
        for operation in WRITES:
            with self.subTest(operation=operation):
                self.reject(lambda r, s, o=operation: claim(s, o).update(
                    authority_ref="checkpoint-3"), "transition_authority")


class Witnesses(Transitions):
    def reject_fields(self, changes, *, operation=FEED_INS, repin=True):
        for field, value in changes.items():
            with self.subTest(field=field):
                self.reject(lambda r, s, f=field, v=value: match(s, operation).update({f: v}),
                            "ownership_match", repin=repin)

    def test_operation_resource_key_and_identity_are_bound(self):
        self.reject_fields({"operation_id": "other-operation", "resource_ref": "setting",
                            "identity_sha256": "0" * 64, "key_sha256": "0" * 64})

    def test_owner_is_bound(self):
        self.reject_fields({"owner_ref": "other-owner"})

    def test_execution_and_nonce_are_bound(self):
        self.reject_fields({"execution_ref": "other-execution", "nonce_ref": "other-nonce"})

    def test_authority_and_resource_epochs_are_bound(self):
        self.reject_fields({"authority_epoch": "8", "resource_epoch": "8"})

    def test_checkpoint_and_method_are_bound(self):
        self.reject_fields({"checkpoint_ref": "checkpoint-3"})
        self.reject_fields({"method": "db_cas"})
        self.reject_fields({"method": "db_delete_owned"}, operation=SETTING_CAS0)

    def test_snapshot_digests_are_bound_without_repin(self):
        self.reject_fields({"before_state_sha256": "0" * 64, "after_state_sha256": "0" * 64},
                           repin=False)

    def test_snapshot_digests_cannot_be_swapped(self):
        def mutate(receipt, records):
            data = match(records, SETTING_CAS0)
            data["before_state_sha256"], data["after_state_sha256"] = (
                data["after_state_sha256"], data["before_state_sha256"])
        self.reject(mutate, "ownership_match", repin=False)

    def test_stale_snapshot_after_state_change_is_refused(self):
        self.reject(lambda r, s: set_state_claim(s, FEED_INS, value_sha256="e" * 64),
                    "ownership_match", repin=False)

    def test_observer_is_bound(self):
        self.reject_fields({"observer_ref": "resource-observer"})

    def test_observed_time_is_bound(self):
        self.reject_fields({"observed_at": at("00:08:01")})

    def test_boundary_and_outcome_are_bound(self):
        self.reject_fields({"boundary": "application_only"})
        self.reject_fields({"boundary": "unknown"})
        self.reject_fields({"outcome": "conflict"})
        self.reject_fields({"outcome": "unknown"})

    def test_every_write_has_its_own_bound_witness(self):
        for operation in WRITES:
            with self.subTest(operation=operation):
                self.reject(lambda r, s, o=operation: match(s, o).update(
                    boundary="application_only"), "ownership_match")


class FinalState(Transitions):
    def test_final_state_must_match_the_last_presence(self):
        self.reject(lambda r, s: set_endpoint(
            r, s, "after-feed", presence="present", version="1", value_sha256="1" * 64),
            "transition_final_state")
        self.reject(lambda r, s: set_endpoint(r, s, "after-rsvp-audit", presence="absent"),
                    "transition_final_state")

    def test_final_state_must_match_the_last_version(self):
        self.reject(lambda r, s: set_endpoint(
            r, s, "after-rsvp-audit", presence="present", version="2", value_sha256="1" * 64),
            "transition_final_state")

    def test_final_state_must_match_the_last_value(self):
        self.reject(lambda r, s: set_endpoint(
            r, s, "after-rsvp-audit", presence="present", version="1", value_sha256="f" * 64),
            "transition_final_state")

    def test_final_state_may_differ_in_time_but_not_precede_the_last_write(self):
        def earlier(receipt, records):
            records["after-rsvp-audit"]["data"]["observed_at"] = at("00:07:00")
        self.reject(earlier, "transition_final_time")
        # The fixture's endpoint is observed at 00:23:00, long after the write.
        _, records, _ = transition_fixture()
        self.assertNotEqual(state(records, AUDIT_INS)["observed_at"],
                            records["after-rsvp-audit"]["data"]["observed_at"])


if __name__ == "__main__":
    unittest.main()
