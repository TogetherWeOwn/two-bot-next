"""Hermetic delivery-trace adversaries; not Discord, SQL, stored-row or send proof.

GitHub Actions is the suite's CI of record; nothing here was run locally unless
disclosed separately. Passing shows only that a fabricated five-event trace
agrees with a fabricated source, case, effect and transaction ledger. This is a
PARTIAL consistency graph: no transition chain, claim-token fencing, SQL result,
history read or complete stored value is modeled, and runtime_preflight stays
unsupported. Semantic negatives repin every derived hash (trace pin, exchange
pin, payload, telemetry, source and input chain) so the trace checker, not a
stale digest, refuses. Where a stronger earlier predicate (schema, ordering,
upstream case/telemetry check) may refuse first, the test asserts only a
Refusal; exact codes are asserted only where the first failing predicate is
predictable. require_all_used() may refuse because inherited sidecars stay
unconsumed in this partial graph.
"""

import copy
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from announcement_delivery_trace_fixtures import (
    CLEANUP_AT, KINDS, READ_OPERATION, READ_TRANSACTION, REPLAY_AT, TRACE,
    delivery_effect, delivery_trace_fixture, move_delivery_write, repin_delivery_trace,
    set_effect,
)
from announcement_feed_exchange_fixtures import EXCHANGE, feed_exchange_fixture, post_operation
from announcement_provenance_fixtures import at, envelope, evidence_index
from announcement_runtime_check import CLAIM_CHECKS
from announcement_runtime_delivery_trace import check_delivery_trace_claims
from announcement_runtime_evidence import canonical_digest
from announcement_runtime_policy import runtime_preflight
from announcement_runtime_schema import REFERENCE_LIMIT, Refusal
from announcement_runtime_source_inputs import SOURCE_PLAN_REF
from announcement_source_input_fixtures import source_input_fixture


def trace(records):
    return records[TRACE]["data"]


def events(records):
    return trace(records)["events"]


def event_of(records, kind):
    return next(x for x in events(records) if x["kind"] == kind)


def operation(receipt, method):
    return delivery_effect(receipt, method)["operation_id"]


class DeliveryTrace(unittest.TestCase):
    def check(self, receipt, records, now):
        return check_delivery_trace_claims(receipt, evidence_index(records), now)

    def reject(self, mutate, code=None, **repin):
        receipt, records, now = delivery_trace_fixture()
        mutate(receipt, records)
        repin_delivery_trace(receipt, records, **repin)
        with self.assertRaises(Refusal) as caught:
            self.check(receipt, records, now)
        if code is not None:
            self.assertEqual(code, caught.exception.code)

    def accept(self, mutate, **repin):
        receipt, records, now = delivery_trace_fixture()
        mutate(receipt, records)
        repin_delivery_trace(receipt, records, **repin)
        self.assertIsNone(self.check(receipt, records, now))


class Golden(DeliveryTrace):
    def test_baseline_five_events_bind_actual_operations_and_stay_bounded(self):
        receipt, records, now = delivery_trace_fixture()
        self.assertEqual(list(KINDS), [x["kind"] for x in events(records)])
        post = post_operation(receipt)["operation_id"]
        self.assertEqual(operation(receipt, "db_insert"), trace(records)["claim_ref"])
        self.assertEqual([operation(receipt, "db_insert"), post, post,
                          operation(receipt, "db_cas"), READ_OPERATION],
                         [x["operation_id"] for x in events(records)])
        self.assertLessEqual(len(records), REFERENCE_LIMIT)
        for name in ("cases", "actual_effects", "expected_effects"):
            self.assertLessEqual(len(receipt["observations"][name]), 256)
        index = evidence_index(records)
        self.assertIsNone(check_delivery_trace_claims(receipt, index, now))
        # Inherited authority sidecars stay unconsumed: complete packets still refuse.
        with self.assertRaisesRegex(Refusal, "unreferenced_evidence$"):
            index.require_all_used()

    def test_checker_runs_last_and_composes_the_feed_exchange_check(self):
        self.assertIs(check_delivery_trace_claims, CLAIM_CHECKS[-1])
        self.reject(lambda r, rec: rec[EXCHANGE]["data"]["response"]["message"].update(content="x"),
                    "feed_exchange_message")

    def test_source_plan_pin_binds_exactly_one_trace_record(self):
        _, records, _ = delivery_trace_fixture()
        self.assertEqual(1, sum(x["kind"] == "feed_delivery_trace" for x in records.values()))
        self.assertEqual({"ref": TRACE, "sha256": canonical_digest(trace(records))},
                         records[SOURCE_PLAN_REF]["data"]["delivery_trace"])

    def test_stale_pin_refuses_before_semantic_checks(self):
        self.reject(lambda r, rec: trace(rec).update(claim_ref="other-claim"),
                    "delivery_trace_content", trace=False)

    def test_source_only_and_exchange_only_fixtures_stay_incomplete(self):
        _, source, _ = source_input_fixture()
        self.assertFalse(any(x["kind"] == "feed_delivery_trace" for x in source.values()))
        self.assertEqual("e" * 64, source[SOURCE_PLAN_REF]["data"]["delivery_trace"]["sha256"])
        receipt, records, now = feed_exchange_fixture()
        self.assertNotIn(TRACE, records)
        with self.assertRaises(Refusal) as caught:
            self.check(receipt, records, now)
        self.assertEqual("unresolved_reference", caught.exception.code)

    def test_runtime_admission_remains_unconditional_hold(self):
        receipt, _, _ = delivery_trace_fixture()
        self.assertFalse(receipt["promotable"])
        with self.assertRaisesRegex(Refusal, "unsupported"):
            runtime_preflight()


class Shape(DeliveryTrace):
    def test_missing_fields_refuse(self):
        def drop(kind, field):
            return lambda r, rec: event_of(rec, kind).pop(field)
        cases = [("trace-" + f, lambda r, rec, f=f: trace(rec).pop(f))
                 for f in ("key_ref", "nonce_ref", "claim_ref", "events")]
        cases += [(kind + "-" + field, drop(kind, field)) for kind, field in (
            ("claim", "row"), ("claim", "operation_id"), ("claim", "observed_at"),
            ("claim", "observer_ref"), ("wire_attempt", "operation_id"),
            ("accepted", "message_id"), ("committed", "row"), ("replay", "row"))]
        for name, mutate in cases:
            with self.subTest(name):
                self.reject(mutate)

    def test_wrong_kinds_refuse(self):
        with self.subTest("record kind"):
            self.reject(lambda r, rec: rec[TRACE].update(kind="feed_post_exchange"))
        with self.subTest("pin points at a different record kind"):
            self.reject(lambda r, rec: rec[SOURCE_PLAN_REF]["data"]["delivery_trace"].update(ref=EXCHANGE),
                        "reference_kind", trace=False)
        with self.subTest("event kind outside the closed five"):
            self.reject(lambda r, rec: event_of(rec, "wire_attempt").update(kind="claimed"))
        with self.subTest("kind without that kind's required fields"):
            self.reject(lambda r, rec: event_of(rec, "wire_attempt").update(kind="committed"))

    def test_unknown_fields_and_model_only_kinds_refuse(self):
        mutations = {
            "trace field": lambda r, rec: trace(rec).update(extra=1),
            "event field": lambda r, rec: event_of(rec, "wire_attempt").update(extra=1),
            "pending row message": lambda r, rec: event_of(rec, "claim")["row"].update(message_id="9201"),
            "delivered row field": lambda r, rec: event_of(rec, "replay")["row"].update(extra=1),
        }
        for kind in ("prewire_failure", "wire_failure", "completion_unknown",
                     "history_unknown", "abort"):
            mutations["kind " + kind] = lambda r, rec, kind=kind: events(rec).append(
                {**copy.deepcopy(events(rec)[1]), "kind": kind})
        for name, mutate in mutations.items():
            with self.subTest(name):
                self.reject(mutate)

    def test_event_count_limits(self):
        with self.subTest("none"):
            self.reject(lambda r, rec: trace(rec).update(events=[]))
        with self.subTest("seventeen"):
            self.reject(lambda r, rec: trace(rec).update(
                events=(copy.deepcopy(events(rec)) * 4)[:17]))
        with self.subTest("sixteen schema-valid repeats still fail semantically"):
            self.reject(lambda r, rec: trace(rec).update(
                events=(copy.deepcopy(events(rec)) * 4)[:16]))

    def test_record_bound_is_exactly_the_reference_limit(self):
        receipt, records, now = delivery_trace_fixture()
        for number in range(REFERENCE_LIMIT - len(records)):
            ref = "pad-%d" % number
            records[ref] = envelope(ref, "feed_delivery_trace", copy.deepcopy(trace(records)))
        self.assertEqual(REFERENCE_LIMIT, len(records))
        self.assertIsNone(self.check(receipt, records, now))
        records["pad-over"] = envelope("pad-over", "feed_delivery_trace", copy.deepcopy(trace(records)))
        with self.assertRaises(Refusal) as caught:
            self.check(receipt, records, now)
        self.assertEqual("evidence_inventory", caught.exception.code)


class Scope(DeliveryTrace):
    def test_run_scope_is_the_attempt(self):
        def mutate(receipt, records):
            records[TRACE]["run"]["run_id"] = str(int(records[TRACE]["run"]["run_id"]) + 1)
        self.reject(mutate, "evidence_attempt")

    def test_key_nonce_and_claim_refs_must_match_the_plan_and_insert(self):
        self.reject(lambda r, rec: trace(rec).update(key_ref="key-feed"), "delivery_trace_scope")
        self.reject(lambda r, rec: trace(rec).update(nonce_ref="other-nonce"), "delivery_trace_scope")
        self.reject(lambda r, rec: trace(rec).update(claim_ref=operation(r, "db_cas")),
                    "delivery_trace_claim")
        self.reject(lambda r, rec: trace(rec).update(claim_ref=READ_OPERATION),
                    "delivery_trace_claim")

    def test_event_operations_must_be_actual_and_right_kind(self):
        def point(kind, target):
            def mutate(receipt, records):
                value = target(receipt) if callable(target) else target
                event_of(records, kind).update(operation_id=value)
            return mutate
        history = lambda r: next(x["operation_id"] for x in r["observations"]["actual_effects"]
                                 if x["method"] == "history_get")
        for name, mutate in (
                ("unknown claim", point("claim", "missing-operation")),
                ("unknown replay", point("replay", "missing-operation")),
                ("claim at cas", point("claim", lambda r: operation(r, "db_cas"))),
                ("wire at insert", point("wire_attempt", lambda r: operation(r, "db_insert"))),
                ("accepted at insert", point("accepted", lambda r: operation(r, "db_insert"))),
                ("committed at insert", point("committed", lambda r: operation(r, "db_insert"))),
                ("replay at cas", point("replay", lambda r: operation(r, "db_cas"))),
                ("replay at history miss", point("replay", history))):
            with self.subTest(name):
                self.reject(mutate, "delivery_trace_operation")

    def test_replay_read_must_belong_to_the_replay_case(self):
        def unassign(receipt, records):
            case = next(x for x in receipt["observations"]["cases"] if x["case_id"] == "feed-replay")
            for field in ("expected_effect_refs", "actual_effect_refs"):
                case[field].remove(READ_OPERATION)
        self.reject(unassign, "delivery_trace_operation")

        def borrow(receipt, records):
            unassign(receipt, records)
            case = next(x for x in receipt["observations"]["cases"] if x["case_id"] == "feed-on")
            for field in ("expected_effect_refs", "actual_effect_refs"):
                case[field].append(READ_OPERATION)
        self.reject(borrow)  # an upstream case-scope predicate may refuse first

    def test_message_ids_must_match_the_exchange_response(self):
        self.reject(lambda r, rec: event_of(rec, "accepted").update(message_id="9999"),
                    "delivery_trace_message")

        def all_three(receipt, records):
            event_of(records, "accepted")["message_id"] = "9999"
            for kind in ("committed", "replay"):
                event_of(records, kind)["row"]["message_id"] = "9999"
        self.reject(all_three, "delivery_trace_message")
        for kind in ("committed", "replay"):
            with self.subTest(kind):
                self.reject(lambda r, rec, kind=kind: event_of(rec, kind)["row"].update(message_id="9999"),
                            "delivery_trace_row")

    def test_nonce_must_match_the_selection(self):
        other = "0" * 24
        self.reject(lambda r, rec: event_of(rec, "claim")["row"].update(nonce=other),
                    "delivery_trace_claim")
        for kind in ("committed", "replay"):
            with self.subTest(kind):
                self.reject(lambda r, rec, kind=kind: event_of(rec, kind)["row"].update(nonce=other),
                            "delivery_trace_row")
        self.reject(lambda r, rec: event_of(rec, "claim")["row"].update(nonce="not-hex"))

    def test_row_states_must_be_pending_then_delivered(self):
        nonce = lambda rec: event_of(rec, "claim")["row"]["nonce"]
        self.reject(lambda r, rec: event_of(rec, "replay").update(
            row={"state": "pending", "nonce": nonce(rec)}), "delivery_trace_row")
        self.reject(lambda r, rec: event_of(rec, "committed").update(
            row={"state": "pending", "nonce": nonce(rec)}), "delivery_trace_row")
        self.reject(lambda r, rec: event_of(rec, "claim").update(
            row=copy.deepcopy(event_of(rec, "replay")["row"])), "delivery_trace_claim")
        for name, row in (("unknown state", {"state": "unknown", "nonce": "0" * 24}),
                          ("empty", {}), ("null", None)):
            with self.subTest(name):
                self.reject(lambda r, rec, row=row: event_of(rec, "replay").update(row=copy.deepcopy(row)))

    def test_observer_must_be_the_coverage_observer(self):
        for kind in KINDS:
            with self.subTest(kind):
                self.reject(lambda r, rec, kind=kind: event_of(rec, kind).update(observer_ref="other-observer"),
                            "delivery_trace_observation")


class Timing(DeliveryTrace):
    def test_event_times_must_match_their_ledger_time(self):
        for kind in KINDS:
            with self.subTest(kind):
                self.reject(lambda r, rec, kind=kind: event_of(rec, kind).update(observed_at=at("00:23:00")),
                            "delivery_trace_observation")

    def test_accepted_time_follows_the_exchange_response_not_the_effect(self):
        stamp = at("00:08:05")

        def move(receipt, records, follow):
            records["platform-" + post_operation(receipt)["operation_id"]]["data"]["observed_at"] = stamp
            records[EXCHANGE]["data"]["response"]["received_at"] = stamp
            if follow:
                event_of(records, "accepted")["observed_at"] = stamp
        self.accept(lambda r, rec: move(r, rec, True))
        self.reject(lambda r, rec: move(r, rec, False), "delivery_trace_observation")

    def test_reordered_or_omitted_events_refuse(self):
        for index in range(len(KINDS) - 1):
            def swap(receipt, records, index=index):
                items = events(records)
                items[index], items[index + 1] = items[index + 1], items[index]
            with self.subTest("swap " + KINDS[index]):
                self.reject(swap)
        for index, kind in enumerate(KINDS):
            with self.subTest("omit " + kind):
                self.reject(lambda r, rec, index=index: events(rec).pop(index))

    def test_duplicated_events_refuse_including_a_second_wire_attempt(self):
        for index, kind in enumerate(KINDS):
            with self.subTest(kind):
                self.reject(lambda r, rec, index=index: events(rec).insert(
                    index + 1, copy.deepcopy(events(rec)[index])))


class Replay(DeliveryTrace):
    def test_read_is_budgeted_separate_and_after_commit(self):
        receipt, records, _ = delivery_trace_fixture()
        entry = next(x for x in receipt["resources"]["entries"] if x["ref"] == "delivery")
        plan = records["runtime-resource-plan"]["data"]
        self.assertIn("db_read", entry["allowed_operations"])
        self.assertEqual(4, entry["expected_count"])
        self.assertEqual(4, entry["maximum_count"])
        self.assertIn({"ref": "budget-delivery-db_read", "resource_ref": "delivery",
                       "method": "db_read", "expected_count": 1, "maximum_count": 1},
                      plan["operation_budgets"])
        self.assertEqual(receipt["resources"]["plan"]["sha256"], canonical_digest(plan))
        self.assertEqual(receipt["authority"]["scope_sha256"],
                         canonical_digest(records["runtime-authority-scope"]["data"]))
        read, cas = delivery_effect(receipt, "db_read"), delivery_effect(receipt, "db_cas")
        self.assertEqual(READ_OPERATION, read["operation_id"])
        self.assertGreater(read["sequence"], cas["sequence"])
        self.assertEqual([], read["db_transition_refs"])
        transaction = records[READ_TRANSACTION]["data"]
        self.assertEqual([READ_OPERATION], [x["operation_id"] for x in transaction["effects"]])
        self.assertEqual([], transaction["transition_refs"])
        self.assertEqual(at(REPLAY_AT), transaction["committed_at"])
        self.assertEqual(at(REPLAY_AT), read["observed_at"])
        self.assertIn(READ_TRANSACTION,
                      receipt["observations"]["coverage"][0]["transaction_receipt_refs"])
        case = next(x for x in receipt["observations"]["cases"] if x["case_id"] == "feed-replay")
        self.assertIn(READ_OPERATION, case["actual_effect_refs"])

    def test_unbudgeted_or_merged_read_refuses(self):
        def unallow(receipt, records):
            entry = next(x for x in receipt["resources"]["entries"] if x["ref"] == "delivery")
            entry["allowed_operations"].remove("db_read")
        with self.subTest("not allowed"):
            self.reject(unallow)

        def unbudget(receipt, records):
            plan = records["runtime-resource-plan"]["data"]
            plan["operation_budgets"] = [x for x in plan["operation_budgets"]
                                         if x["ref"] != "budget-delivery-db_read"]
        with self.subTest("no plan budget"):
            self.reject(unbudget)

        def merge(receipt, records):
            cas = "transaction-" + operation(receipt, "db_cas")
            records[cas]["data"]["effects"].append(records[READ_TRANSACTION]["data"]["effects"][0])
            receipt["observations"]["coverage"][0]["transaction_receipt_refs"].remove(READ_TRANSACTION)
            del records[READ_TRANSACTION]
        with self.subTest("read shares the completion transaction"):
            self.reject(merge)

    def test_delivery_cleanup_moves_after_replay_and_only_for_delivery(self):
        receipt, records, _ = delivery_trace_fixture()
        delete = delivery_effect(receipt, "db_delete_owned")
        read = delivery_effect(receipt, "db_read")
        self.assertEqual(at(CLEANUP_AT), delete["observed_at"])
        self.assertGreater(delete["sequence"], read["sequence"])
        self.assertEqual(at(CLEANUP_AT), records[delete["db_transition_refs"][0]]["data"]["claim"]["observed_at"])
        self.assertEqual(at(CLEANUP_AT), records["transaction-" + delete["operation_id"]]["data"]["committed_at"])
        others = [x for x in receipt["observations"]["actual_effects"]
                  if x["method"] == "db_delete_owned" and x["resource_ref"] != "delivery"]
        self.assertTrue(others)
        self.assertEqual({at("00:08:00")}, {x["observed_at"] for x in others})
        self.assertEqual(receipt["observations"]["actual_effects"],
                         receipt["observations"]["expected_effects"])

    def test_intervening_release_refuses(self):
        self.reject(lambda r, rec: move_delivery_write(r, rec, "db_delete_owned", "00:09:00"),
                    "delivery_trace_interference")

    def test_reclaim_style_write_between_claim_and_replay_refuses(self):
        # Reclassifying the cleanup as a mid-window CAS may be refused upstream
        # (unassigned case effect or budget) before reaching interference.
        def reclaim(receipt, records):
            move_delivery_write(receipt, records, "db_delete_owned", "00:09:00")
            effect = delivery_effect(receipt, "db_delete_owned")
            records[effect["db_transition_refs"][0]]["data"]["method"] = "db_cas"
            set_effect(receipt, effect["operation_id"], method="db_cas")
        self.reject(reclaim)

    def test_uncertain_wire_or_effect_outcomes_refuse(self):
        post = lambda r: post_operation(r)["operation_id"]
        for outcome in ("unknown", "uncertain", "failed"):
            with self.subTest("post " + outcome):
                self.reject(lambda r, rec, outcome=outcome: set_effect(r, post(r), outcome=outcome))
            with self.subTest("read transaction " + outcome):
                self.reject(lambda r, rec, outcome=outcome: records_tx(rec).update(outcome=outcome))


def records_tx(records):
    return records[READ_TRANSACTION]["data"]


class Independence(DeliveryTrace):
    def test_fixture_calls_return_equal_but_independent_objects(self):
        first, second = delivery_trace_fixture(), delivery_trace_fixture()
        self.assertEqual(first, second)
        for left, right in zip(first[:2], second[:2]):
            self.assertIsNot(left, right)
        events(first[1])[0]["row"]["nonce"] = "0" * 24
        first[0]["observations"]["actual_effects"].pop()
        first[1][SOURCE_PLAN_REF]["data"]["delivery_trace"]["sha256"] = "f" * 64
        self.assertEqual(second, delivery_trace_fixture())

    def test_checker_does_not_mutate_receipt_or_sidecars(self):
        receipt, records, now = delivery_trace_fixture()
        before = copy.deepcopy((receipt, records))
        self.assertIsNone(self.check(receipt, records, now))
        self.assertEqual(before, (receipt, records))


if __name__ == "__main__":
    unittest.main()
