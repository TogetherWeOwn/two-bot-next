"""Hermetic case/effect-correspondence adversaries; not stored-data or lifecycle proof.

Passing here shows only that fabricated per-case operation lists agree with a
fabricated effect ledger under the scoped profile (four feed windows plus the
declared interaction cases). Nothing is observed, authenticated or executed and
runtime_preflight stays unsupported. Case-list mutations are checked directly by
check_case_effect_claims, so those refusals use its exact codes. Mutations that
upstream scheduler/telemetry checks might also catch first assert only that a
Refusal is raised (code=None); the exact code is not guessed.
"""

import copy
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import announcement_case_fixtures
from announcement_case_fixtures import case_fixture, resequence
from announcement_provenance_fixtures import at, evidence_index
from announcement_telemetry_fixtures import repin_telemetry
from announcement_runtime_cases import check_case_effect_claims
from announcement_runtime_policy import runtime_preflight
from announcement_runtime_schema import Refusal

ACK_ON = "additional-ack-6"
EDIT = "budget-control-channel-interaction_edit-0"
MEMBER_PRE = "budget-actor-member_get-0"
MEMBER_POST = "budget-actor-member_get-1"
SOURCE_OFF = "budget-source-source_get-0"
SOURCE_ON = "budget-source-source_get-1"
SOURCE_REPLAY = "budget-source-source_get-2"
HISTORY = "budget-feed-channel-history_get-0"
CLAIM = "budget-delivery-db_insert-0"
POST = "budget-feed-channel-feed_post-0"
COMPLETE = "budget-delivery-db_cas-0"
RSVP = "budget-rsvp-db_insert-0"
OFF_RSVP_ACK = "additional-ack-4"


def case_of(receipt, case_id):
    return next(c for c in receipt["observations"]["cases"] if c["case_id"] == case_id)


def effect_of(receipt, operation, ledger="actual_effects"):
    return next(x for x in receipt["observations"][ledger] if x["operation_id"] == operation)


def drop(receipt, case_id, operation):
    case = case_of(receipt, case_id)
    for field in ("expected_effect_refs", "actual_effect_refs"):
        case[field].remove(operation)


def add(receipt, case_id, operation):
    case = case_of(receipt, case_id)
    for field in ("expected_effect_refs", "actual_effect_refs"):
        case[field].append(operation)


def retime(receipt, records, operation, clock):
    """Move one effect and its mirrored claims, then refresh derived state."""
    effect = effect_of(receipt, operation)
    effect["observed_at"] = at(clock)
    if effect["method"].startswith("db_"):
        for ref in effect["db_transition_refs"]:
            records[ref]["data"]["claim"]["observed_at"] = at(clock)
    else:
        records["wire-" + operation]["data"]["observed_at"] = at(clock)
        platform = records.get("platform-" + operation)
        if platform is not None:
            platform["data"]["observed_at"] = at(clock)
    resequence(receipt, records)
    actual = receipt["observations"]["actual_effects"]
    for record in records.values():
        if record["kind"] == "db_transaction":
            members = {x["operation_id"] for x in record["data"]["effects"]}
            record["data"]["committed_at"] = next(
                x["observed_at"] for x in actual if x["operation_id"] in members)
    repin_telemetry(receipt, records)


class Cases(unittest.TestCase):
    def check(self, receipt, records, now):
        return check_case_effect_claims(receipt, evidence_index(records), now)

    def reject(self, mutate, code=None):
        receipt, records, now = case_fixture()
        mutate(receipt, records)
        with self.assertRaises(Refusal) as caught:
            self.check(receipt, records, now)
        if code is not None:
            self.assertEqual(code, caught.exception.code)


class Coherence(Cases):
    def test_consistent_claims_are_bounded_and_not_runtime_authority(self):
        receipt, records, now = case_fixture()
        observations = receipt["observations"]
        self.assertLessEqual(len(records), 256)
        for name in ("cases", "actual_effects", "expected_effects"):
            self.assertLessEqual(len(observations[name]), 256)
        for case in observations["cases"]:
            self.assertLessEqual(len(case["actual_effect_refs"]), 256)
        index = evidence_index(records)
        self.assertIsNone(check_case_effect_claims(receipt, index, now))
        # This scoped checker does not consume or qualify inherited authority
        # sidecars. Keep complete-packet accounting adverse, never fake usage.
        with self.assertRaisesRegex(Refusal, "unreferenced_evidence$"):
            index.require_all_used()
        self.assertFalse(receipt["promotable"])
        with self.assertRaisesRegex(Refusal, "unsupported"):
            runtime_preflight()

    def test_fixture_is_explicitly_synthetic_and_not_lifecycle_proof(self):
        self.assertIn("SYNTHETIC", announcement_case_fixtures.__doc__)
        self.assertIn("not stored-data or lifecycle proof", announcement_case_fixtures.__doc__)
        receipt, _, _ = case_fixture()
        # Setup/cleanup business-neutral operations stay collapsed at one instant.
        self.assertEqual(at("00:08:00"), effect_of(receipt, "budget-app-gateway_identify-0")["observed_at"])
        self.assertEqual(effect_of(receipt, POST)["observed_at"], at("00:08:00"))

    def test_expected_ledger_equals_actual_ledger_and_sequence_is_dense(self):
        receipt, _, _ = case_fixture()
        actual = receipt["observations"]["actual_effects"]
        self.assertEqual(actual, receipt["observations"]["expected_effects"])
        self.assertEqual(list(range(1, len(actual) + 1)), [x["sequence"] for x in actual])

    def test_replay_and_final_off_have_no_business_write(self):
        receipt, _, _ = case_fixture()
        for case_id, allowed in (("feed-replay", {SOURCE_REPLAY, HISTORY}), ("feed-final_off", set())):
            self.assertEqual(allowed, set(case_of(receipt, case_id)["actual_effect_refs"]))

    def test_each_declared_operation_is_assigned_to_exactly_one_case(self):
        receipt, _, _ = case_fixture()
        refs = [ref for case in receipt["observations"]["cases"] for ref in case["actual_effect_refs"]]
        self.assertEqual(len(refs), len(set(refs)))


class Inventory(Cases):
    def test_expected_and_actual_references_must_match(self):
        self.reject(lambda r, s: case_of(r, "feed-on")["expected_effect_refs"].remove(SOURCE_ON),
                    "case_effect_inventory")
        self.reject(lambda r, s: case_of(r, "feed-on")["actual_effect_refs"].remove(SOURCE_ON),
                    "case_effect_inventory")
        self.reject(lambda r, s: case_of(r, "feed-off")["actual_effect_refs"].append(HISTORY),
                    "case_effect_inventory")
        self.reject(lambda r, s: case_of(r, "feed-off")["expected_effect_refs"].reverse() or
                    case_of(r, "feed-off")["expected_effect_refs"].append(HISTORY),
                    "case_effect_inventory")

    def test_unknown_operation_refuses(self):
        self.reject(lambda r, s: add(r, "feed-off", "ghost-operation"), "case_effect_inventory")
        self.reject(lambda r, s: case_of(r, "feed-off")["actual_effect_refs"].append("ghost-operation"),
                    "case_effect_inventory")

    def test_reference_duplicated_within_a_case_refuses(self):
        self.reject(lambda r, s: add(r, "feed-on", POST), "reference_duplicate")
        for field in ("actual_effect_refs", "expected_effect_refs"):
            self.reject(lambda r, s: case_of(r, "feed-on")[field].append(SOURCE_ON), "reference_duplicate")
        self.reject(lambda r, s: case_of(r, "feed-on")["resource_refs"].append("source"),
                    "reference_duplicate")

    def test_reference_duplicated_across_cases_refuses(self):
        # Another case's own check always trips first (scope, resources or
        # operation counts), so only that some Refusal occurs is asserted.
        for other, ref in (("feed-off", SOURCE_ON), ("feed-replay", SOURCE_OFF),
                           ("off-rsvp", "additional-ack-0"), ("on-rsvp", "additional-ack-0")):
            with self.subTest(other=other):
                self.reject(lambda r, s: add(r, other, ref))

    def test_wrong_case_resources_refuse(self):
        self.reject(lambda r, s: case_of(r, "off-rsvp")["resource_refs"].append("rsvp"), "case_resources")
        self.reject(lambda r, s: case_of(r, "on-rsvp")["resource_refs"].remove("event"), "interaction_resource")
        self.reject(lambda r, s: case_of(r, "on-rsvp")["resource_refs"].remove("rsvp-audit"),
                    "case_effect_scope")
        self.reject(lambda r, s: case_of(r, "feed-on")["resource_refs"].remove("source"), "case_effect_scope")
        self.reject(lambda r, s: case_of(r, "feed-on")["resource_refs"].remove("setting"), "case_resources")
        self.reject(lambda r, s: case_of(r, "feed-off")["resource_refs"].append("actor"), "case_resources")
        self.reject(lambda r, s: case_of(r, "feed-final_off")["resource_refs"].clear(), "array_size")

    def test_case_outcome_or_coverage_other_than_pass_refuses(self):
        for outcome in ("unknown", "blocked", "fail"):
            with self.subTest(outcome=outcome):
                self.reject(lambda r, s: case_of(r, "feed-on").update(outcome=outcome),
                            "enum" if outcome == "blocked" else "case_coverage")
        self.reject(lambda r, s: case_of(r, "feed-off").update(coverage_ref="other-coverage"), "case_coverage")

    def test_unassigned_business_operation_refuses(self):
        for case_id, ref in (("feed-replay", HISTORY), ("feed-off", SOURCE_OFF),
                             ("feed-replay", SOURCE_REPLAY)):
            with self.subTest(ref=ref):
                self.reject(lambda r, s: drop(r, case_id, ref), "unassigned_case_effect")


class Scope(Cases):
    def test_effect_at_exclusive_case_end_refuses(self):
        self.reject(lambda r, s: retime(r, s, SOURCE_OFF, "00:07:00"), "case_effect_scope")
        self.reject(lambda r, s: retime(r, s, HISTORY, "00:11:00"), "case_effect_scope")

    def test_effect_before_case_start_refuses(self):
        self.reject(lambda r, s: retime(r, s, SOURCE_ON, "00:06:59"), "case_effect_scope")

    def test_platform_readback_at_or_after_case_end_refuses(self):
        for operation, clock in ((POST, "00:09:00"), (POST, "00:09:30"),
                                 ("budget-control-channel-interaction_ack-0", "00:07:00")):
            with self.subTest(operation=operation, clock=clock):
                self.reject(lambda r, s: s["platform-" + operation]["data"].update(observed_at=at(clock)),
                            "case_platform_scope")

    def test_platform_readback_just_inside_case_end_is_accepted(self):
        receipt, records, now = case_fixture()
        records["platform-" + POST]["data"]["observed_at"] = at("00:08:59")
        self.assertIsNone(self.check(receipt, records, now))

    def test_effect_at_inclusive_case_start_is_accepted(self):
        receipt, records, now = case_fixture()
        retime(receipt, records, SOURCE_OFF, "00:05:00")
        self.assertIsNone(self.check(receipt, records, now))

    def test_wrong_served_pair_refuses(self):
        def wrong(r, s):
            effect_of(r, SOURCE_OFF)["served_ref"] = "served-two"
            resequence(r, s)
        self.reject(wrong)
        self.reject(lambda r, s: case_of(r, "feed-off").update(served_ref="served-two"))

    def test_actual_and_expected_ledger_disagreement_refuses(self):
        def diverge(r, s):
            # Expected/actual timestamps are independently bounded but need not
            # match; payload identity IS part of exact multiset reconciliation.
            effect_of(r, SOURCE_OFF, "expected_effects")["payload_sha256"] = "b" * 64
        self.reject(diverge, "effect_multiset")


class FeedInventory(Cases):
    def test_wrong_or_extra_feed_case_inventory_refuses(self):
        self.reject(lambda r, s: r["observations"]["cases"].remove(case_of(r, "feed-final_off")))
        self.reject(lambda r, s: r["observations"]["cases"].remove(case_of(r, "feed-replay")))

        def extra(r, s):
            clone = copy.deepcopy(case_of(r, "feed-off"))
            clone.update(case_id="feed-extra", expected_effect_refs=[], actual_effect_refs=[])
            r["observations"]["cases"].append(clone)
        self.reject(extra)

        def duplicate(r, s):
            r["observations"]["cases"].append(copy.deepcopy(case_of(r, "feed-off")))
        self.reject(duplicate, "case_inventory")

    def test_feed_case_with_wrong_phase_or_input_refuses(self):
        self.reject(lambda r, s: case_of(r, "feed-off").update(phase="on"))
        self.reject(lambda r, s: case_of(r, "feed-on").update(input_ref="interaction-plan-ref"))

    def test_missing_positive_feed_references_refuse(self):
        self.reject(lambda r, s: drop(r, "feed-on", SOURCE_ON), "case_source_read")
        for ref in (CLAIM, POST, COMPLETE):
            with self.subTest(ref=ref):
                self.reject(lambda r, s: drop(r, "feed-on", ref), "case_feed_effects")

    def test_wrong_phase_feed_business_effect_refuses(self):
        def move(r, s):
            drop(r, "feed-on", POST)
            add(r, "feed-replay", POST)
        self.reject(move, "case_feed_effects")

        def replay_only(r, s):
            add(r, "feed-replay", POST)
        self.reject(replay_only)

        def final(r, s):
            add(r, "feed-final_off", COMPLETE)
        self.reject(final)

    def test_feed_case_with_interaction_effect_refuses(self):
        self.reject(lambda r, s: add(r, "feed-off", "additional-ack-0"))


class Interactions(Cases):
    def test_off_interaction_borrowing_rsvp_write_refuses(self):
        self.reject(lambda r, s: add(r, "off-rsvp", RSVP), "case_effect_scope")

        def widen(r, s):
            case_of(r, "off-rsvp")["resource_refs"].extend(("rsvp", "rsvp-audit"))
            add(r, "off-rsvp", RSVP)
        self.reject(widen, "case_effect_scope")

        def borrow(r, s):
            drop(r, "on-rsvp", RSVP)
            add(r, "off-rsvp", RSVP)
        self.reject(borrow)

    def test_ack_or_edit_omitted_refuses(self):
        self.reject(lambda r, s: drop(r, "on-rsvp", ACK_ON), "case_interaction_effects")
        self.reject(lambda r, s: drop(r, "on-rsvp", EDIT), "case_interaction_effects")
        self.reject(lambda r, s: drop(r, "off-rsvp", OFF_RSVP_ACK), "case_interaction_effects")
        for case_id, operation in (("off-feed-add", "budget-control-channel-interaction_ack-0"),
                                   ("off-feed-remove", "additional-ack-2")):
            with self.subTest(case_id=case_id):
                self.reject(lambda r, s: drop(r, case_id, operation), "case_interaction_effects")

    def test_positive_interaction_missing_reads_or_writes_refuses(self):
        for ref in (MEMBER_PRE, MEMBER_POST, "budget-event-event_get-0", "budget-event-event_get-1",
                    RSVP, "budget-rsvp-audit-db_insert-0"):
            with self.subTest(ref=ref):
                self.reject(lambda r, s: drop(r, "on-rsvp", ref), "case_interaction_effects")

    def test_off_interaction_with_extra_ack_refuses(self):
        self.reject(lambda r, s: add(r, "off-rsvp", "additional-ack-0"))


class Ordering(Cases):
    def test_feed_claim_must_precede_post_and_post_must_precede_completion(self):
        self.reject(lambda r, s: retime(r, s, CLAIM, "00:08:05"), "case_operation_order")
        self.reject(lambda r, s: retime(r, s, COMPLETE, "00:07:55"), "case_operation_order")

    def test_source_read_after_claim_refuses(self):
        self.reject(lambda r, s: retime(r, s, SOURCE_ON, "00:07:55"), "case_operation_order")

    def test_interaction_edit_before_ack_refuses(self):
        self.reject(lambda r, s: retime(r, s, EDIT, "00:07:20"), "case_operation_order")

    def test_ack_after_pre_read_refuses(self):
        self.reject(lambda r, s: retime(r, s, ACK_ON, "00:07:45"), "case_operation_order")

    def test_pre_read_after_write_or_post_read_before_write_refuses(self):
        self.reject(lambda r, s: retime(r, s, MEMBER_PRE, "00:08:05"), "case_operation_order")
        self.reject(lambda r, s: retime(r, s, MEMBER_POST, "00:07:45"), "case_operation_order")

    def test_edit_before_post_read_refuses(self):
        self.reject(lambda r, s: retime(r, s, EDIT, "00:08:10"), "case_operation_order")

    def test_case_list_order_does_not_hide_reversed_sequence(self):
        def shuffled(r, s):
            case = case_of(r, "feed-on")
            case["actual_effect_refs"].reverse()
            case["expected_effect_refs"].reverse()
        receipt, records, now = case_fixture()
        shuffled(receipt, records)
        self.assertIsNone(self.check(receipt, records, now))
        self.reject(lambda r, s: (shuffled(r, s), retime(r, s, CLAIM, "00:08:05")), "case_operation_order")


class UnknownOrBlocked(Cases):
    def test_unknown_or_blocked_effect_outcome_refuses(self):
        for outcome in ("unknown", "blocked", "rejected"):
            with self.subTest(outcome=outcome):
                def mutate(r, s):
                    effect_of(r, SOURCE_OFF)["outcome"] = outcome
                    resequence(r, s)
                self.reject(mutate)

    def test_unknown_or_blocked_counts_refuse(self):
        def mutate(r, s):
            effect = effect_of(r, POST)
            effect["accepted_count"] = 0
            effect["outcome"] = "unknown"
            resequence(r, s)
        self.reject(mutate)


if __name__ == "__main__":
    unittest.main()
