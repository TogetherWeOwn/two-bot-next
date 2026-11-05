"""Hermetic lifecycle ledger adversaries; not cleanup, drain or recovery proof.

Passing here shows only that fabricated, unauthenticated setup/cleanup claims,
db_transition sidecars and endpoint state snapshots agree with a tiny fabricated
effect ledger (one database resource). It is deliberately PARTIAL coverage: the
opaque drain, queue, in-flight, postcondition, readiness, restoration authority
and verification references, the ownership witnesses and the in-window write
semantics are NOT consumed by the checker and have no sidecars here, so a
complete packet must still refuse them elsewhere. The checker is not patched or
mocked, composes no upstream checker and returns no capability;
runtime_preflight stays unsupported.
"""

import copy
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from announcement_lifecycle_fixtures import (
    CLEANUP, INTERIOR, NOW, OTHER_VALUE, READ, SETUP, absent, effect,
    identity, lifecycle_fixture, present, state_record,
)
from announcement_provenance_fixtures import at, envelope, evidence_index
from announcement_runtime_evidence import canonical_digest
from announcement_runtime_lifecycle import check_lifecycle_ledger_claims
from announcement_runtime_policy import runtime_preflight
from announcement_runtime_schema import Refusal

OPAQUE = ("drain-ref", "queue-baseline-ref", "queue-final-ref", "restoration-authority-ref",
          "restoration-verification-ref", "postcondition-ref", "readiness-after-ref",
          "snapshot-ref", "compatibility-ref", "artifact-ref")


def effects(receipt):
    return receipt["observations"]["actual_effects"]


def find(receipt, operation):
    return next(x for x in effects(receipt) if x["operation_id"] == operation)


def set_claim(receipt, records, operation, **changes):
    """Change a ledger claim and its db_transition sidecar claim together."""
    for field in ("setup", "cleanup"):
        for item in receipt["lifecycle"][field]:
            if item["operation_id"] == operation:
                item.update(changes)
    records["transition-" + operation]["data"]["claim"].update(changes)


def retime(receipt, records, operation, clock):
    find(receipt, operation)["observed_at"] = at(clock)
    set_claim(receipt, records, operation, observed_at=at(clock))
    if operation == CLEANUP:
        records["state-final"]["data"]["observed_at"] = at(clock)


def set_method(receipt, records, operation, method):
    find(receipt, operation)["method"] = method
    records["transition-" + operation]["data"]["method"] = method


def add_write(receipt, records, operation, sequence, method, clock):
    """Append an unlisted db write WITH a coherent sidecar claim."""
    effects(receipt).append(effect(operation, sequence, method, clock))
    claim = copy.deepcopy(records["transition-" + INTERIOR]["data"]["claim"])
    claim.update(operation_id=operation, observed_at=at(clock),
                 ownership_match_ref="match-" + operation)
    records["transition-" + operation] = envelope("transition-" + operation, "db_transition", {
        "claim": claim, "transaction_ref": "transaction-" + operation, "method": method})


def set_state(receipt, records, ref, claim):
    records[ref]["data"]["claim"] = copy.deepcopy(claim)
    for item in receipt["resources"]["states"]:
        if item["ref"] == ref:
            item.clear()
            item.update(claim)


def add_second_setup(receipt, records):
    """Insert a second setup write (sequence 2, 00:03) with its own sidecar."""
    first = find(receipt, SETUP)
    second = copy.deepcopy(first)
    second.update(operation_id="op-setup2", sequence=2, observed_at=at("00:03:00"),
                  db_transition_refs=["transition-op-setup2"])
    for item in effects(receipt)[1:]:
        item["sequence"] += 1
    effects(receipt).insert(1, second)
    claim = copy.deepcopy(receipt["lifecycle"]["setup"][0])
    claim.update(operation_id="op-setup2", observed_at=at("00:03:00"),
                 ownership_match_ref="match-op-setup2")
    receipt["lifecycle"]["setup"].append(claim)
    records["transition-op-setup2"] = envelope("transition-op-setup2", "db_transition", {
        "claim": copy.deepcopy(claim), "transaction_ref": "transaction-setup2",
        "method": records["transition-" + SETUP]["data"]["method"]})


def add_idle(receipt, records, before, after):
    """Add a second database resource that no effect writes."""
    ident = identity()
    ident.update(table="feed_relays", key_sha256="9" * 64)
    entry = copy.deepcopy(receipt["resources"]["entries"][0])
    entry.update(ref="db-idle", identity=ident, before_state_ref="idle-before",
                 after_state_ref="idle-after")
    receipt["resources"]["entries"].append(entry)
    for ref, claim, clock in (("idle-before", before, "00:00:30"),
                              ("idle-after", after, "00:22:00")):
        record = state_record({**claim, "ref": ref}, clock)
        record["data"].update(resource_ref="db-idle", identity_sha256=canonical_digest(ident))
        records[ref] = record


class Lifecycle(unittest.TestCase):
    def check(self, receipt, records, now=NOW):
        return check_lifecycle_ledger_claims(receipt, evidence_index(records), now)

    def accept(self, mutate=None, prior="absent"):
        receipt, records, now = lifecycle_fixture(prior)
        if mutate:
            mutate(receipt, records)
        self.assertIsNone(self.check(receipt, records, now))

    def reject(self, mutate, code, prior="absent"):
        receipt, records, now = lifecycle_fixture(prior)
        mutate(receipt, records)
        with self.assertRaisesRegex(Refusal, "announcement evidence refused: " + code + "$"):
            self.check(receipt, records, now)


class Coherence(Lifecycle):
    def test_absent_prior_ledger_is_coherent_and_grants_no_capability(self):
        receipt, records, now = lifecycle_fixture("absent")
        self.assertIsNone(self.check(receipt, records, now))
        self.assertFalse(receipt["promotable"])
        with self.assertRaisesRegex(Refusal, "unsupported"):
            runtime_preflight()

    def test_present_prior_restores_value_with_a_new_version_token(self):
        receipt, records, now = lifecycle_fixture("present")
        before, after = (records[x]["data"]["claim"] for x in ("state-before", "state-after"))
        self.assertNotEqual(before["version"], after["version"])
        self.assertEqual(before["value_sha256"], after["value_sha256"])
        self.assertIsNone(self.check(receipt, records, now))

    def test_opaque_lifecycle_references_are_not_consumed(self):
        for prior in ("absent", "present"):
            receipt, records, _ = lifecycle_fixture(prior)
            life = receipt["lifecycle"]
            self.assertTrue(set(OPAQUE) & {life[k] for k in life if isinstance(life[k], str)})
            self.assertFalse(set(OPAQUE) & set(records))
            self.accept(prior=prior)

    def test_opaque_references_may_change_freely_without_effect(self):
        def mutate(receipt, records):
            life = receipt["lifecycle"]
            life.update(drain_ref="other-drain", queue_final_ref="other-queue",
                        postcondition_refs=["other-post"], restoration_verification_ref="other")
        self.accept(mutate)

    def test_every_sidecar_is_resolved_by_the_narrow_check(self):
        # Accounting for these narrow records only; NOT full packet verification.
        for prior in ("absent", "present"):
            receipt, records, now = lifecycle_fixture(prior)
            index = evidence_index(records)
            check_lifecycle_ledger_claims(receipt, index, now)
            index.require_all_used()

    def test_extra_well_formed_sidecar_is_preserved_as_unreferenced(self):
        receipt, records, now = lifecycle_fixture()
        records["state-extra"] = state_record(absent("state-extra"), "00:00:30")
        index = evidence_index(records)
        self.assertIsNone(check_lifecycle_ledger_claims(receipt, index, now))
        with self.assertRaisesRegex(Refusal, "unreferenced_evidence$"):
            index.require_all_used()

    def test_non_database_resources_are_ignored_by_state_checks(self):
        def mutate(receipt, records):
            synthetic = copy.deepcopy(receipt["resources"]["entries"][0])
            synthetic.update(ref="synth-resource", kind="synthetic", identity={
                "kind": "synthetic", "job_id": "77", "actor_ref": "actor-ref",
                "request_sha256": "a" * 64, "resource_refs": ["synth-resource"],
                "cleanup_plan_ref": "cleanup-plan-ref"},
                before_state_ref="missing", after_state_ref="missing")
            receipt["resources"]["entries"].append(synthetic)
        self.accept(mutate)


class LedgerCorrespondence(Lifecycle):
    def test_duplicate_claim_in_one_ledger_refuses(self):
        self.reject(lambda r, _: r["lifecycle"]["setup"].append(
            copy.deepcopy(r["lifecycle"]["setup"][0])), "reference_duplicate")

    def test_unknown_operation_refuses(self):
        self.reject(lambda r, _: r["lifecycle"]["setup"][0].update(operation_id="op-missing"),
                    "lifecycle_operation")

    def test_cleanup_may_not_reuse_a_setup_operation(self):
        def mutate(receipt, records):
            receipt["lifecycle"]["cleanup"] = [copy.deepcopy(receipt["lifecycle"]["setup"][0])]
        self.reject(mutate, "lifecycle_operation_time", prior="present")

    def test_setup_may_not_use_a_delete(self):
        self.reject(lambda r, rec: set_method(r, rec, SETUP, "db_delete_owned"),
                    "lifecycle_operation")

    def test_cleanup_may_not_use_an_insert(self):
        self.reject(lambda r, rec: set_method(r, rec, CLEANUP, "db_insert"),
                    "lifecycle_operation")

    def test_wire_effect_in_cleanup_ledger_refuses(self):
        def mutate(receipt, records):
            receipt["observations"]["actual_effects"].append(
                effect("op-wire", 5, "feed_post", "00:20:45", transition=False))
            wire = copy.deepcopy(receipt["lifecycle"]["cleanup"][0])
            wire.update(operation_id="op-wire", observed_at=at("00:20:45"))
            receipt["lifecycle"]["cleanup"].append(wire)
        self.reject(mutate, "lifecycle_operation")

    def test_effect_needs_exactly_one_transition_ref(self):
        self.reject(lambda r, _: find(r, SETUP).update(db_transition_refs=[]),
                    "lifecycle_operation")
        self.reject(lambda r, _: find(r, SETUP).update(
            db_transition_refs=["transition-" + SETUP, "transition-" + CLEANUP]),
            "lifecycle_operation")

    def test_interior_write_also_needs_exactly_one_transition_ref(self):
        self.reject(lambda r, _: find(r, INTERIOR).update(db_transition_refs=[]),
                    "lifecycle_operation")
        self.reject(lambda r, _: find(r, INTERIOR).update(db_transition_refs=["transition-none"]),
                    "unresolved_reference")

    def test_interior_write_sidecar_must_bind_its_effect(self):
        self.reject(lambda r, rec: set_claim(r, rec, INTERIOR, outcome="conflict"),
                    "lifecycle_transition")
        self.reject(lambda _, rec: rec["transition-" + INTERIOR]["data"].update(method="db_insert"),
                    "lifecycle_transition")

    def test_unknown_or_mistyped_transition_ref_refuses(self):
        self.reject(lambda r, _: find(r, SETUP).update(db_transition_refs=["transition-none"]),
                    "unresolved_reference")
        self.reject(lambda r, _: find(r, SETUP).update(db_transition_refs=["state-before"]),
                    "reference_kind")

    def test_sidecar_claim_must_equal_the_ledger_claim(self):
        self.reject(lambda _, rec: rec["transition-" + SETUP]["data"]["claim"].update(
            key_sha256="f" * 64), "lifecycle_transition")

    def test_sidecar_method_must_equal_the_effect_method(self):
        self.reject(lambda _, rec: rec["transition-" + CLEANUP]["data"].update(method="db_insert"),
                    "lifecycle_transition")

    def test_ledger_and_sidecar_agreeing_on_a_bad_field_still_refuses(self):
        for changes in ({"outcome": "conflict"}, {"actual_count": 2}, {"expected_count": 2},
                        {"key_sha256": "f" * 64}, {"owner": "other-owner"},
                        {"intended_after_state_ref": "state-other"},
                        {"observed_at": at("00:02:30")}, {"resource_ref": "db-none"}):
            with self.subTest(changes=changes):
                self.reject(lambda r, rec: set_claim(r, rec, SETUP, **changes),
                            "lifecycle_transition")

    def test_effect_resource_must_match_the_claim_resource(self):
        self.reject(lambda r, _: find(r, SETUP).update(resource_ref="other-ref"),
                    "lifecycle_transition")

    def test_ledger_must_follow_effect_sequence(self):
        def reverse(receipt, records):
            add_second_setup(receipt, records)
            receipt["lifecycle"]["setup"].reverse()
        self.accept(add_second_setup)
        self.reject(reverse, "lifecycle_ledger_order")

    def test_duplicate_effect_operation_or_resource_inventory_refuses(self):
        def replay(receipt, _):
            again = copy.deepcopy(find(receipt, READ))
            again.update(sequence=5, observed_at=at("00:25:00"))
            effects(receipt).append(again)
        self.reject(replay, "operation_replay")
        self.reject(lambda r, _: r["resources"]["entries"].append(
            copy.deepcopy(r["resources"]["entries"][0])), "resource_inventory")


class OrderAndWindows(Lifecycle):
    def test_sequence_gap_and_time_order_refuse(self):
        self.reject(lambda r, _: find(r, CLEANUP).update(sequence=6), "sequence_gap")
        self.reject(lambda r, _: find(r, INTERIOR).update(observed_at=at("00:11:30")),
                    "sequence_time")

    def test_setup_must_precede_the_rollout(self):
        self.reject(lambda r, rec: retime(r, rec, SETUP, "00:06:00"), "lifecycle_operation_time")
        self.reject(lambda r, rec: retime(r, rec, SETUP, "00:00:30"), "lifecycle_operation_time")

    def test_cleanup_window_is_inclusive_at_observation_end_and_restoration(self):
        self.accept(lambda r, rec: retime(r, rec, CLEANUP, "00:20:00"))
        self.accept(lambda r, rec: retime(r, rec, CLEANUP, "00:21:00"))
        self.reject(lambda r, rec: retime(r, rec, CLEANUP, "00:19:59"), "lifecycle_operation_time")
        self.reject(lambda r, rec: retime(r, rec, CLEANUP, "00:21:01"), "lifecycle_operation_time")

    def test_snapshot_rollout_and_restoration_must_bracket_the_test_window(self):
        life = lambda r: r["lifecycle"]
        self.reject(lambda r, _: life(r)["served_before"].update(observed_at=at("00:06:00")),
                    "lifecycle_window")
        self.reject(lambda r, _: life(r)["served_after"].update(observed_at=at("00:19:00")),
                    "lifecycle_window")
        self.reject(lambda r, _: life(r)["served_after"].update(observed_at=at("00:31:00")),
                    "lifecycle_window")
        receipt, records, _ = lifecycle_fixture()
        with self.assertRaisesRegex(Refusal, "lifecycle_window$"):
            self.check(receipt, records, NOW.replace(minute=29))

    def test_adverse_abort_or_unresolved_in_flight_refuses(self):
        self.reject(lambda r, _: r["lifecycle"].update(abort="fenced"), "lifecycle_adverse")
        self.reject(lambda r, _: r["lifecycle"].update(in_flight="unknown"), "lifecycle_adverse")

    def test_every_effect_must_be_one_accepted_in_attempt_effect(self):
        self.reject(lambda r, _: find(r, READ).update(outcome="failed", accepted_count=0),
                    "lifecycle_effect")
        self.reject(lambda r, _: effects(r).append(
            effect("op-late-read", 5, "db_read", "00:35:00", transition=False)),
            "lifecycle_effect")

    def test_evidence_must_be_for_the_receipt_run(self):
        def mutate(receipt, _):
            receipt["attempt"]["run"]["attempt"] = 2
        self.reject(mutate, "evidence_attempt")


class UnlistedWrites(Lifecycle):
    def test_interior_write_is_allowed_but_not_assigned(self):
        receipt, records, _ = lifecycle_fixture()
        self.assertIn(INTERIOR, [x["operation_id"] for x in effects(receipt)])
        self.assertIn("transition-" + INTERIOR, records)
        self.assertNotIn(INTERIOR, [x["operation_id"] for f in ("setup", "cleanup")
                                    for x in receipt["lifecycle"][f]])
        self.accept()

    def test_unlisted_write_at_observation_start_is_interior(self):
        def mutate(receipt, records):
            find(receipt, READ).update(observed_at=at("00:11:00"))
            retime(receipt, records, INTERIOR, "00:11:00")
        self.accept(mutate)

    def test_unlisted_write_at_exclusive_observation_end_refuses(self):
        self.reject(lambda r, rec: retime(r, rec, INTERIOR, "00:20:00"),
                    "unassigned_lifecycle_write")

    def test_unlisted_write_before_observation_start_refuses(self):
        def mutate(receipt, records):
            find(receipt, READ).update(observed_at=at("00:10:30"))
            retime(receipt, records, INTERIOR, "00:10:30")
        self.reject(mutate, "unassigned_lifecycle_write")

    def test_unlisted_write_after_restoration_refuses(self):
        self.reject(lambda r, rec: add_write(r, rec, "op-late", 5, "db_insert", "00:25:00"),
                    "unassigned_lifecycle_write")

    def test_message_cleanup_is_unsupported(self):
        self.reject(lambda r, _: effects(r).append(
            effect("op-message", 5, "message_delete_owned", "00:25:00", transition=False)),
            "unsupported_message_cleanup")

    def test_reads_never_need_a_ledger_entry(self):
        self.accept(lambda r, _: effects(r).append(
            effect("op-late-read", 5, "db_read", "00:25:00", transition=False)))


class StateRestoration(Lifecycle):
    def test_present_prior_with_a_different_value_refuses(self):
        self.reject(lambda r, rec: set_state(r, rec, "state-after", present(
            "state-after", "3", OTHER_VALUE)), "lifecycle_prior_state", prior="present")

    def test_absent_and_present_never_substitute_even_with_equal_digest(self):
        self.reject(lambda r, rec: set_state(r, rec, "state-after", present("state-after", "3")),
                    "lifecycle_prior_state")
        self.reject(lambda r, rec: set_state(r, rec, "state-after", absent("state-after")),
                    "lifecycle_prior_state", prior="present")

    def test_final_state_must_equal_the_endpoint_ignoring_only_the_ref(self):
        self.reject(lambda r, rec: set_state(r, rec, "state-final", present("state-final", "4")),
                    "lifecycle_final_state", prior="present")
        self.reject(lambda r, rec: set_state(r, rec, "state-final", present(
            "state-final", "3", OTHER_VALUE)), "lifecycle_final_state", prior="present")
        self.reject(lambda r, rec: set_state(r, rec, "state-final", present("state-final", "1")),
                    "lifecycle_final_state")

    def test_last_transition_state_must_be_observed_at_commit(self):
        for clock in ("00:01:00", "00:23:00", "00:41:00"):
            with self.subTest(clock=clock):
                self.reject(lambda _, rec: rec["state-final"]["data"].update(observed_at=at(clock)),
                            "lifecycle_final_time")

    def test_state_must_bind_resource_identity_and_owner(self):
        for field, value in (("identity_sha256", "f" * 64), ("owner_ref", "other-owner"),
                             ("resource_ref", "other-ref")):
            for ref in ("state-before", "state-after", "state-final"):
                with self.subTest(field=field, ref=ref):
                    self.reject(lambda _, rec: rec[ref]["data"].update({field: value}),
                                "lifecycle_state")
        self.reject(lambda _, rec: rec["state-before"]["data"]["claim"].update(ref="state-x"),
                    "lifecycle_state")

    def test_endpoint_state_times_must_bracket_the_lifecycle(self):
        self.reject(lambda _, rec: rec["state-before"]["data"].update(observed_at=at("00:01:30")),
                    "lifecycle_state_time")
        self.reject(lambda _, rec: rec["state-after"]["data"].update(observed_at=at("00:20:59")),
                    "lifecycle_state_time")
        self.reject(lambda _, rec: rec["state-after"]["data"].update(observed_at=at("00:31:00")),
                    "lifecycle_state_time")

    def test_missing_or_mistyped_endpoint_state_refuses(self):
        self.reject(lambda _, rec: rec.pop("state-after"), "unresolved_reference")
        self.reject(lambda r, _: r["resources"]["entries"][0].update(
            before_state_ref="transition-" + SETUP), "reference_kind")

    def test_resource_without_writes_must_be_unchanged_exactly(self):
        same = present("x", "1")
        self.accept(lambda r, rec: add_idle(r, rec, same, same))
        self.accept(lambda r, rec: add_idle(r, rec, absent("x"), absent("x")))
        self.reject(lambda r, rec: add_idle(r, rec, same, present("x", "1", OTHER_VALUE)),
                    "lifecycle_prior_state")
        self.reject(lambda r, rec: add_idle(r, rec, same, present("x", "2")),
                    "lifecycle_unobserved_change")
        self.reject(lambda r, rec: add_idle(r, rec, absent("x"), same), "lifecycle_prior_state")


if __name__ == "__main__":
    unittest.main()
