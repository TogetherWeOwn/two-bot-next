"""Hermetic first-RSVP storage adversaries; not SQL, transaction or database proof.

GitHub Actions is the suite's CI of record. Passing shows only that fabricated
row/audit projections, two fabricated present states and one fabricated shared
transaction agree with a fabricated effect ledger. No SQL ran, no stored value
was read and runtime_preflight stays unsupported. The real interaction, case,
telemetry and resource checkers run unmocked on the real interaction fixture.
Negatives repin the other derived claims (state digests, exchange pin, bodies,
telemetry and input chain) so the storage checker, not a stale hash, refuses;
the stale-digest cases opt out explicitly. Where intake schema refuses first the
test asserts only a Refusal. Exact codes are asserted only where predictable.
Cleanup and restoration of the stored rows are deliberately not qualified.
"""

import copy
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from announcement_interaction_fixtures import (
    EXCHANGES, PLAN, RESPONDED_AT, TRANSACTION, interaction_fixture, repin_interactions,
    rsvp_inserts, rsvp_state_refs, rsvp_storage_fixture)
from announcement_provenance_fixtures import at, evidence_index
from announcement_runtime_evidence import canonical_digest
from announcement_runtime_policy import runtime_preflight
from announcement_runtime_rsvp_storage import check_rsvp_storage_claims
from announcement_runtime_schema import REFERENCE_LIMIT, Refusal

STATUSES = ("going", "interested", "declined")
ROW_FIELDS = ("guild_id", "event_id", "user_id", "status", "responded_at")
AUDIT_FIELDS = ("id", "guild_id", "actor_id", "action", "target_key", "outcome",
                "reason", "created_at")
OTHER_TRANSACTION = "transaction-budget-feed-db_insert-0"


def storage(records):
    return records[EXCHANGES]["data"]["storage"]


def state_of(receipt, records, name):
    return records[rsvp_state_refs(receipt, records)[0 if name == "rsvp" else 1]]["data"]


def transition_of(receipt, records, name):
    effect = rsvp_inserts(receipt, records)[0 if name == "rsvp" else 1]
    return records[effect["db_transition_refs"][0]]["data"]


def entry_of(receipt, ref):
    return next(x for x in receipt["resources"]["entries"] if x["ref"] == ref)


def set_responded(records, value):
    """Move the shared response/audit timestamp coherently."""
    storage(records)["rsvp"]["responded_at"] = value
    storage(records)["audit"]["created_at"] = value


def set_outcome(records, status):
    storage(records)["rsvp"]["status"] = status
    storage(records)["audit"]["outcome"] = status


class Storage(unittest.TestCase):
    def check(self, receipt, records, now):
        return check_rsvp_storage_claims(receipt, evidence_index(records), now)

    def reject(self, mutate, code=None, **repin):
        receipt, records, now = rsvp_storage_fixture()
        mutate(receipt, records)
        repin_interactions(receipt, records, **repin)
        with self.assertRaises(Refusal) as caught:
            self.check(receipt, records, now)
        if code is not None:
            self.assertEqual(code, caught.exception.code)

    def accept(self, mutate, **repin):
        receipt, records, now = rsvp_storage_fixture()
        mutate(receipt, records)
        repin_interactions(receipt, records, **repin)
        self.assertIsNone(self.check(receipt, records, now))


class Coherence(Storage):
    def test_baseline_is_bounded_returns_none_and_is_not_runtime_authority(self):
        receipt, records, now = rsvp_storage_fixture()
        self.assertLessEqual(len(records), REFERENCE_LIMIT)
        self.assertEqual(256, REFERENCE_LIMIT)
        for name in ("cases", "actual_effects", "expected_effects"):
            self.assertLessEqual(len(receipt["observations"][name]), 256)
        index = evidence_index(records)
        self.assertIsNone(check_rsvp_storage_claims(receipt, index, now))
        # Inherited authority sidecars stay unconsumed: complete packets still refuse.
        with self.assertRaisesRegex(Refusal, "unreferenced_evidence$"):
            index.require_all_used()
        self.assertFalse(receipt["promotable"])
        with self.assertRaisesRegex(Refusal, "unsupported"):
            runtime_preflight()

    def test_extra_unreferenced_evidence_still_refuses_all_used_accounting(self):
        receipt, records, now = rsvp_storage_fixture()
        state_ref = rsvp_state_refs(receipt, records)[0]
        extra = copy.deepcopy(records[state_ref])
        extra["ref"] = extra["data"]["claim"]["ref"] = "state-extra"
        records["state-extra"] = extra
        index = evidence_index(records)
        self.assertIsNone(check_rsvp_storage_claims(receipt, index, now))
        with self.assertRaisesRegex(Refusal, "unreferenced_evidence$"):
            index.require_all_used()

    def test_each_status_is_coherent_across_input_request_response_and_storage(self):
        for status in STATUSES:
            with self.subTest(status=status):
                receipt, records, now = rsvp_storage_fixture(status)
                self.assertIsNone(self.check(receipt, records, now))
                self.assertEqual(status, records["runtime-input-plan"]["data"]["rsvp"]["status"])
                exchange = next(x for x in records[EXCHANGES]["data"]["exchanges"]
                                if x["case_id"] == "on-rsvp")
                self.assertIn({"name": "status", "value": status}, exchange["request"]["options"])
                edit = next(x for x in exchange["responses"] if x["route"] == "original_response")
                self.assertEqual("RSVP saved: " + status + ".", edit["readback"]["content"])
                self.assertIn("RSVP saved: " + status + ".", edit["body_utf8"])
                self.assertEqual(status, storage(records)["rsvp"]["status"])
                self.assertEqual(status, storage(records)["audit"]["outcome"])
                for name in ("rsvp", "audit"):
                    self.assertEqual(canonical_digest(storage(records)[name]),
                                     state_of(receipt, records, name)["claim"]["value_sha256"])

    def test_storage_follows_c_construction_of_the_first_response(self):
        receipt, records, _ = rsvp_storage_fixture()
        row, audit = storage(records)["rsvp"], storage(records)["audit"]
        plan = next(x for x in records[PLAN]["data"]["interactions"] if x["case_id"] == "on-rsvp")
        self.assertEqual("on-rsvp", storage(records)["case_id"])
        self.assertEqual("rsvp:" + plan["interaction_id"], audit["id"])
        self.assertEqual({"kind": "sql_null"}, audit["reason"])
        self.assertEqual("event.rsvp", audit["action"])
        self.assertEqual((row["event_id"], row["user_id"], row["guild_id"], row["status"]),
                         (audit["target_key"], audit["actor_id"], audit["guild_id"],
                          audit["outcome"]))
        self.assertEqual(row["responded_at"], audit["created_at"])
        self.assertEqual(at(RESPONDED_AT), row["responded_at"])

    def test_shared_interaction_fixture_has_storage_but_no_state_sidecars(self):
        receipt, shared, now = interaction_fixture()
        _, enriched, _ = rsvp_storage_fixture()
        refs = rsvp_state_refs(receipt, shared)
        self.assertFalse(set(refs) & shared.keys())
        self.assertEqual(set(refs), enriched.keys() - shared.keys())
        self.assertEqual(shared[EXCHANGES]["data"], enriched[EXCHANGES]["data"])
        with self.assertRaises(Refusal):
            self.check(receipt, shared, now)

    def test_one_shared_transaction_and_two_distinct_present_states(self):
        receipt, records, _ = rsvp_storage_fixture()
        coverage = receipt["observations"]["coverage"][0]
        self.assertEqual(TRANSACTION, storage(records)["transaction_ref"])
        self.assertIn(TRANSACTION, coverage["transaction_receipt_refs"])
        operations = {x["operation_id"] for x in records[TRANSACTION]["data"]["effects"]}
        self.assertTrue({x["operation_id"] for x in rsvp_inserts(receipt, records)} <= operations)
        refs = rsvp_state_refs(receipt, records)
        baselines = {entry_of(receipt, ref)[field] for ref in ("rsvp", "rsvp-audit")
                     for field in ("before_state_ref", "after_state_ref")}
        self.assertEqual(2, len(set(refs)))
        self.assertFalse(set(refs) & baselines)
        for name, ref in zip(("rsvp", "audit"), refs):
            state = state_of(receipt, records, name)
            self.assertEqual("present", state["claim"]["presence"])
            self.assertEqual(ref, state["claim"]["ref"])
            self.assertEqual(records[TRANSACTION]["data"]["committed_at"], state["observed_at"])
            claim = transition_of(receipt, records, name)["claim"]
            self.assertEqual(ref, claim["intended_after_state_ref"])
            self.assertEqual(ref, claim["actual_after_state_ref"])

    def test_exchange_pin_covers_the_storage_claims(self):
        receipt, records, _ = rsvp_storage_fixture()
        before = records[PLAN]["data"]["exchanges"]["sha256"]
        self.assertEqual(canonical_digest(records[EXCHANGES]["data"]), before)
        storage(records)["rsvp"]["status"] = "declined"
        repin_interactions(receipt, records)
        self.assertNotEqual(before, records[PLAN]["data"]["exchanges"]["sha256"])

    def test_cleanup_and_restoration_are_not_qualified(self):
        def scramble(receipt, records):
            effect = next(x for x in receipt["observations"]["actual_effects"]
                          if x["resource_ref"] == "rsvp" and x["method"] == "db_delete_owned")
            claim = records[effect["db_transition_refs"][0]]["data"]["claim"]
            claim["intended_after_state_ref"] = claim["actual_after_state_ref"] = "state-elsewhere"
        self.accept(scramble)


class Composition(Storage):
    def test_interaction_exchange_check_runs_first(self):
        def both(receipt, records):
            next(x for x in records[EXCHANGES]["data"]["exchanges"]
                 if x["case_id"] == "on-rsvp")["request"]["interaction_id"] = "7"
            storage(records)["rsvp"]["guild_id"] = "7"
        self.reject(both, "interaction_request")

    def test_stale_exchange_pin_is_refused_before_storage(self):
        def move(receipt, records):
            set_responded(records, at("00:07:51"))
        self.reject(move, "interaction_exchange_content", pin=False, states=False)

    def test_telemetry_transaction_and_transition_checks_are_composed(self):
        def rolled_back(receipt, records):
            records[TRANSACTION]["data"]["outcome"] = "rolled_back"
        self.reject(rolled_back)

        def diverge(receipt, records):
            transition_of(receipt, records, "rsvp")["claim"]["actual_after_state_ref"] = (
                entry_of(receipt, "rsvp")["after_state_ref"])
        self.reject(diverge)

    def test_input_status_alone_is_refused_when_request_and_storage_disagree(self):
        def input_only(receipt, records):
            records["runtime-input-plan"]["data"]["rsvp"]["status"] = "interested"
        self.reject(input_only)


class Scope(Storage):
    def test_storage_must_name_the_one_positive_rsvp_case(self):
        for case_id in ("off-feed-add", "feed-on", "no-such-case"):
            with self.subTest(case_id=case_id):
                self.reject(lambda r, s, c=case_id: storage(s).update(case_id=c),
                            "rsvp_storage_scope")


class Literals(Storage):
    def test_row_literals_must_equal_the_key_and_request(self):
        for field in ("guild_id", "event_id", "user_id"):
            with self.subTest(field=field):
                self.reject(lambda r, s, f=field: storage(s)["rsvp"].update({f: "7"}),
                            "rsvp_storage_row")

    def test_row_status_must_match_the_input_plan_even_when_audit_agrees(self):
        self.reject(lambda r, s: set_outcome(s, "declined"), "rsvp_storage_row")

    def test_audit_fields_must_derive_from_the_row(self):
        for field, value in (("id", "rsvp:910"), ("guild_id", "7"), ("actor_id", "7"),
                             ("target_key", "7"), ("outcome", "declined"),
                             ("created_at", at("00:07:51"))):
            with self.subTest(field=field):
                self.reject(lambda r, s, f=field, v=value: storage(s)["audit"].update({f: v}),
                            "rsvp_storage_audit")

    def test_audit_timestamp_may_not_diverge_from_the_row(self):
        self.reject(lambda r, s: storage(s)["rsvp"].update(responded_at=at("00:07:51")),
                    "rsvp_storage_audit")


class Transactions(Storage):
    def test_transaction_must_be_a_covered_transaction_holding_both_inserts(self):
        for ref in (OTHER_TRANSACTION, "transaction-not-covered"):
            with self.subTest(ref=ref):
                self.reject(lambda r, s, x=ref: storage(s).update(transaction_ref=x),
                            "rsvp_storage_transaction")

    def test_transaction_ref_must_resolve_to_a_transaction_record(self):
        def retarget(receipt, records):
            coverage = receipt["observations"]["coverage"][0]
            coverage["transaction_receipt_refs"].append("key-rsvp")
            records[coverage["ref"]]["data"]["claim"] = copy.deepcopy(coverage)
            storage(records)["transaction_ref"] = "key-rsvp"
        self.reject(retarget)


class Timing(Storage):
    def test_response_time_boundaries_are_inclusive_on_whole_seconds(self):
        for clock in ("00:07:40", "00:08:00"):
            with self.subTest(clock=clock):
                self.accept(lambda r, s, c=clock: set_responded(s, at(c)))

    def test_response_may_not_precede_the_pre_reads_or_follow_the_commit(self):
        for clock in ("00:07:39", "00:08:01", "00:00:05"):
            with self.subTest(clock=clock):
                self.reject(lambda r, s, c=clock: set_responded(s, at(c)), "rsvp_storage_time")

    def test_state_observation_must_equal_the_commit_timestamp(self):
        for clock in ("00:07:59", "00:08:01"):
            with self.subTest(clock=clock):
                self.reject(lambda r, s, c=clock: state_of(r, s, "rsvp").update(observed_at=at(c)),
                            "rsvp_storage_binding")


class States(Storage):
    def test_state_identity_resource_owner_and_observer_are_exact(self):
        for name, field, value in (("rsvp", "resource_ref", "rsvp-audit"),
                                   ("audit", "resource_ref", "rsvp"),
                                   ("rsvp", "identity_sha256", "f" * 64),
                                   ("audit", "owner_ref", "resource-observer"),
                                   ("rsvp", "observer_ref", "other-observer")):
            with self.subTest(name=name, field=field):
                self.reject(lambda r, s, n=name, f=field, v=value: state_of(r, s, n).update({f: v}),
                            "rsvp_storage_binding")

    def test_state_claim_ref_must_equal_the_sidecar_ref(self):
        self.reject(lambda r, s: state_of(r, s, "rsvp")["claim"].update(ref="state-other"),
                    "rsvp_storage_binding")

    def test_state_must_be_present_with_a_matching_projection_digest(self):
        def absent(receipt, records):
            ref = rsvp_state_refs(receipt, records)[0]
            state_of(receipt, records, "rsvp")["claim"] = {"presence": "absent", "ref": ref}
        self.reject(absent, "rsvp_storage_digest")
        self.reject(lambda r, s: state_of(r, s, "audit")["claim"].update(value_sha256="0" * 64),
                    "rsvp_storage_digest", states=False)

    def test_swapped_projection_digests_refuse(self):
        def swap(receipt, records):
            row, audit = (state_of(receipt, records, n)["claim"] for n in ("rsvp", "audit"))
            row["value_sha256"], audit["value_sha256"] = audit["value_sha256"], row["value_sha256"]
        self.reject(swap, "rsvp_storage_digest", states=False)

    def test_coherent_value_edit_without_state_repin_refuses_on_digest(self):
        self.reject(lambda r, s: set_responded(s, at("00:07:51")),
                    "rsvp_storage_digest", states=False)
        self.accept(lambda r, s: set_responded(s, at("00:07:51")))

    def test_non_positive_or_malformed_versions_refuse_at_intake(self):
        for version in ("0", "-1", "1.0", "", 1):
            with self.subTest(version=version):
                self.reject(lambda r, s, v=version: state_of(r, s, "rsvp")["claim"].update(
                    version=v))

    def test_after_states_may_not_reuse_baseline_placeholders_or_each_other(self):
        def reuse(receipt, records, name, ref):
            claim = transition_of(receipt, records, name)["claim"]
            claim["intended_after_state_ref"] = claim["actual_after_state_ref"] = ref
        for name, resource in (("rsvp", "rsvp"), ("audit", "rsvp-audit")):
            for field in ("before_state_ref", "after_state_ref"):
                with self.subTest(name=name, field=field):
                    self.reject(lambda r, s, n=name, x=resource, f=field: reuse(
                        r, s, n, entry_of(r, x)[f]), "rsvp_storage_transition")
        with self.subTest(shared="audit-reuses-rsvp-state"):
            self.reject(lambda r, s: reuse(r, s, "audit", rsvp_state_refs(r, s)[0]),
                        "rsvp_storage_transition")

    def test_transition_must_chain_from_the_resource_baseline(self):
        def rebase(receipt, records):
            transition_of(receipt, records, "rsvp")["claim"]["before_state_ref"] = (
                entry_of(receipt, "rsvp")["after_state_ref"])
        self.reject(rebase, "rsvp_storage_transition")

    def test_state_refs_must_resolve_to_state_records(self):
        def point(receipt, records, ref):
            claim = transition_of(receipt, records, "rsvp")["claim"]
            claim["intended_after_state_ref"] = claim["actual_after_state_ref"] = ref
        self.reject(lambda r, s: point(r, s, "key-rsvp"), "reference_kind")
        self.reject(lambda r, s: point(r, s, "state-missing"), "unresolved_reference")
        self.reject(lambda r, s: s.pop(rsvp_state_refs(r, s)[1]), "unresolved_reference")


class Schema(Storage):
    def test_storage_is_mandatory_and_closed(self):
        def drop(receipt, records):
            del records[EXCHANGES]["data"]["storage"]
        self.reject(drop, "object_fields")
        for value in (None, [], "", {}):
            with self.subTest(value=value):
                self.reject(lambda r, s, v=value: s[EXCHANGES]["data"].update(storage=v))
        self.reject(lambda r, s: storage(s).update(extra="x"), "object_fields")

    def test_every_field_is_required_non_null_and_closed(self):
        for part, fields in (("rsvp", ROW_FIELDS), ("audit", AUDIT_FIELDS)):
            for field in fields:
                with self.subTest(part=part, field=field, mode="missing"):
                    self.reject(lambda r, s, p=part, f=field: storage(s)[p].pop(f),
                                "object_fields")
                with self.subTest(part=part, field=field, mode="null"):
                    self.reject(lambda r, s, p=part, f=field: storage(s)[p].update({f: None}))
            with self.subTest(part=part, mode="extra"):
                self.reject(lambda r, s, p=part: storage(s)[p].update(unknown="x"),
                            "object_fields")
        for field in ("case_id", "transaction_ref", "rsvp", "audit"):
            with self.subTest(top=field):
                self.reject(lambda r, s, f=field: storage(s).pop(f), "object_fields")

    def test_unknown_literals_and_malformed_values_refuse(self):
        cases = (("rsvp", "status", "maybe"), ("rsvp", "guild_id", "0"), ("rsvp", "user_id", 300),
                 ("rsvp", "responded_at", "2026-01-01T00:07:50"), ("audit", "action", "event.other"),
                 ("audit", "outcome", "maybe"), ("audit", "id", ""), ("audit", "id", "bad id"),
                 ("audit", "created_at", "2026-01-01 00:07:50Z"))
        for part, field, value in cases:
            with self.subTest(part=part, field=field, value=value):
                self.reject(lambda r, s, p=part, f=field, v=value: storage(s)[p].update({f: v}))

    def test_reason_must_be_the_explicit_sql_null_marker(self):
        for value in (None, "sql_null", "", {}, {"kind": "empty"}, {"kind": "sql_null", "x": 1},
                      {"kind": None}):
            with self.subTest(reason=value):
                self.reject(lambda r, s, v=value: storage(s)["audit"].update(reason=v))

    def test_malformed_case_and_transaction_refs_refuse(self):
        for field, value in (("case_id", "On-Rsvp"), ("case_id", 5),
                             ("transaction_ref", "Transaction"), ("transaction_ref", "")):
            with self.subTest(field=field, value=value):
                self.reject(lambda r, s, f=field, v=value: storage(s).update({f: v}))


if __name__ == "__main__":
    unittest.main()
