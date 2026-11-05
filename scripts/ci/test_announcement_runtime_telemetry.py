"""Hermetic telemetry-correspondence adversaries; not observation or staging proof.

Passing here shows only that fabricated full-window wire/platform/transaction
claims agree with a fabricated effect ledger. The fixture's single-instant
effects are accounting examples, not lifecycle or stored-case evidence, and
runtime_preflight stays unsupported. Mutations are repinned so a refusal comes
from the semantic predicate under test, not from an incidental stale digest.
"""

import copy
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from announcement_provenance_fixtures import at, evidence_index
from announcement_resource_fixtures import repin_resources
from announcement_authority_fixtures import principal
from announcement_telemetry_fixtures import (
    PLATFORM_METHODS, add_rsvp_read, binding, repin_telemetry, telemetry_fixture,
)
from announcement_runtime_authority import check_authority_graph_claims
from announcement_runtime_evidence import TELEMETRY_RECORDS
from announcement_runtime_policy import runtime_preflight
from announcement_runtime_provenance import check_provenance_claims
from announcement_runtime_resources import check_resource_graph_claims
from announcement_runtime_schema import Refusal
from announcement_runtime_telemetry import check_telemetry_claims

IDENT = "budget-app-gateway_identify-0"
ACK0 = "budget-control-channel-interaction_ack-0"
ACK1 = "budget-control-channel-interaction_ack-1"
EDIT = "budget-control-channel-interaction_edit-0"
FEED = "budget-feed-channel-feed_post-0"
RSVP = "budget-rsvp-db_insert-0"
AUDIT = "budget-rsvp-audit-db_insert-0"
DELIVERY = "budget-delivery-db_insert-0"
CAS = "budget-setting-db_cas-0"
RSVP_TX = "transaction-rsvp-audit"
OTHER_RUN_ATTEMPT = 2


def find(records, kind, operation):
    return next(r["data"] for r in records.values()
                if r["kind"] == kind and r["data"].get("operation_id") == operation)


def effect_of(receipt, operation, ledger="actual_effects"):
    return next(x for x in receipt["observations"][ledger] if x["operation_id"] == operation)


def edit(receipt, operation, **changes):
    """Change one effect coherently in both independently supplied ledgers."""
    for ledger in ("expected_effects", "actual_effects"):
        effect_of(receipt, operation, ledger).update(copy.deepcopy(changes))


def coverage(receipt):
    return receipt["observations"]["coverage"][0]


def transaction(records, ref):
    return records[ref]["data"]


def set_observer(receipt, records, ref):
    coverage(receipt)["observer_ref"] = ref
    for record in records.values():
        if record["kind"] in ("wire_attempt", "platform_effect", "db_transaction"):
            record["data"]["observer_ref"] = ref


class Telemetry(unittest.TestCase):
    def check(self, receipt, records, now):
        return check_telemetry_claims(receipt, evidence_index(records), now)

    def reject(self, mutate, code=None, *, repin=True, after=None):
        receipt, records, now = telemetry_fixture()
        mutate(receipt, records)
        if repin:
            repin_telemetry(receipt, records)
        if after:
            after(receipt, records)
        with self.assertRaisesRegex(Refusal, code or "announcement evidence refused"):
            self.check(receipt, records, now)

    def accept(self, mutate):
        receipt, records, now = telemetry_fixture()
        mutate(receipt, records)
        repin_telemetry(receipt, records)
        self.assertIsNone(self.check(receipt, records, now))


class Coherence(Telemetry):
    def test_consistent_claims_are_not_runtime_authority(self):
        receipt, records, now = telemetry_fixture()
        index = evidence_index(records)
        self.assertIsNone(check_telemetry_claims(receipt, index, now))
        self.assertIsNone(check_provenance_claims(receipt, index, now))
        self.assertIsNone(check_resource_graph_claims(receipt, index, now))
        self.assertIsNone(check_authority_graph_claims(receipt, index, now))
        index.require_all_used()
        self.assertFalse(receipt["promotable"])
        with self.assertRaisesRegex(Refusal, "unsupported"):
            runtime_preflight()

    def test_fixture_is_exact_window_with_one_transition_per_database_effect(self):
        receipt, records, _ = telemetry_fixture()
        attempt, cover = receipt["attempt"], coverage(receipt)
        self.assertEqual((attempt["started_at"], attempt["ended_at"]),
                         (cover["started_at"], cover["ended_at"]))
        self.assertEqual((at("00:00:00"), at("00:24:00")),
                         (attempt["started_at"], attempt["ended_at"]))
        job = records["job-observer"]["data"]
        self.assertEqual((at("00:00:00"), at("00:24:00")), (job["started_at"], job["ended_at"]))
        effects = receipt["observations"]["actual_effects"]
        self.assertEqual(effects, receipt["observations"]["expected_effects"])
        database = [x for x in effects if x["method"].startswith("db_")]
        wire = [x for x in effects if not x["method"].startswith("db_")]
        self.assertEqual(len(wire), len(cover["wire_receipt_refs"]))
        self.assertTrue(all(len(x["db_transition_refs"]) == 1 for x in database))
        self.assertEqual(len(database), len({x["db_transition_refs"][0] for x in database}))
        sizes = sorted(len(transaction(records, ref)["effects"])
                       for ref in cover["transaction_receipt_refs"])
        self.assertEqual([1] * (len(database) - 2) + [2], sizes)
        self.assertEqual([RSVP, AUDIT], [x["operation_id"]
                                         for x in transaction(records, RSVP_TX)["effects"]])
        platform = [x for x in effects if x["method"] in PLATFORM_METHODS]
        self.assertEqual(len(platform), len(cover["platform_receipt_refs"]))
        self.assertTrue(all(x["platform_id_refs"] for x in platform))
        ids = [(x["method"], i) for x in platform for i in x["platform_id_refs"]]
        self.assertEqual(len(ids), len(set(ids)))
        self.assertEqual([receipt["fixtures"]["nonce_ref"]], effect_of(receipt, FEED)["nonce_ref"])

    def test_repin_is_idempotent_and_touches_only_derived_claims(self):
        receipt, records, _ = telemetry_fixture()
        before = copy.deepcopy(records)
        repin_telemetry(receipt, records)
        self.assertEqual(before, records)

    def test_cross_method_platform_identifier_reuse_is_not_a_replay(self):
        def reuse(receipt, records):
            identifier = effect_of(receipt, ACK0)["platform_id_refs"]
            edit(receipt, EDIT, platform_id_refs=identifier)
            find(records, "platform_effect", EDIT)["platform_ids"] = list(identifier)
        self.accept(reuse)


class Inventory(Telemetry):
    def test_all_telemetry_sidecar_kinds_are_closed_and_nonempty(self):
        _, records, _ = telemetry_fixture()
        examples = {r["kind"]: ref for ref, r in records.items() if r["kind"] in TELEMETRY_RECORDS}
        self.assertEqual(set(examples), set(TELEMETRY_RECORDS))
        for kind, ref in examples.items():
            for data in ({}, {**records[ref]["data"], "extra": True}, None):
                with self.subTest(kind=kind, data=type(data)):
                    self.reject(lambda r, s: s[ref].update(data=copy.deepcopy(data)), repin=False)

    def test_reference_lists_cannot_use_the_larger_effect_ledger_limit(self):
        refs = ["operation-" + str(i) for i in range(257)]
        for ref, field in (("coverage-ref", "operation_ids"),
                           (RSVP_TX, "transition_refs")):
            with self.subTest(field=field):
                self.reject(lambda r, s: s[ref]["data"].update({field: refs}),
                            "array_size", repin=False)

    def test_missing_wrong_kind_or_other_attempt_always_refuses(self):
        for ref in ("coverage-ref", "wire-" + ACK0, "wire-" + FEED, "platform-" + FEED,
                    "platform-" + EDIT, "transaction-" + DELIVERY, RSVP_TX,
                    "transition-" + RSVP, "transition-" + CAS, "observer-ref"):
            with self.subTest(ref=ref):
                self.reject(lambda r, s: s.pop(ref), "unresolved_reference", repin=False)
                self.reject(lambda r, s: s[ref].update(kind="document"), repin=False)
                self.reject(lambda r, s: s[ref]["run"].update(attempt=OTHER_RUN_ATTEMPT),
                            "evidence_attempt", repin=False)

    def test_coverage_cardinality_unknown_keys_and_mixed_attempt_refuse(self):
        self.reject(lambda r, s: r["observations"]["coverage"].append(
            copy.deepcopy(coverage(r))), "telemetry_window_inventory")
        self.reject(lambda r, s: r["observations"]["coverage"].clear(),
                    "array_size", repin=False)
        self.reject(lambda r, s: coverage(r).update(unknown_key="x"))
        self.reject(lambda r, s: coverage(r)["run"].update(attempt=OTHER_RUN_ATTEMPT),
                    "provenance_attempt")
        self.reject(lambda r, s: s["coverage-ref"]["data"]["claim"].update(unknown_key="x"),
                    repin=False)

    def test_window_claim_and_operation_list_must_equal_receipt_exactly(self):
        for name, mutate in (
                ("missing", lambda ids: ids.pop()),
                ("duplicate", lambda ids: ids.append(ids[0])),
                ("reordered", lambda ids: ids.reverse()),
                ("unknown", lambda ids: ids.append("ghost-operation"))):
            with self.subTest(name=name):
                self.reject(lambda r, s: None, "telemetry_operations", repin=False,
                            after=lambda r, s: mutate(s["coverage-ref"]["data"]["operation_ids"]))
        self.reject(lambda r, s: None, "telemetry_operations", repin=False,
                    after=lambda r, s: s["coverage-ref"]["data"]["claim"].update(last_sequence=1))

    def test_full_window_gaps_and_incomplete_results_refuse(self):
        for field, value in (("started_at", at("00:00:01")), ("ended_at", at("00:23:59")),
                             ("started_at", at("23:59:59").replace("2026-01-01", "2025-12-31")),
                             ("ended_at", at("00:24:30"))):
            with self.subTest(field=field, value=value):
                self.reject(lambda r, s: coverage(r).update({field: value}), "telemetry_full_window")
        for result in ("gap", "truncated", "sampled", "unknown"):
            with self.subTest(result=result):
                self.reject(lambda r, s: coverage(r).update(result=result), "telemetry")
        for field, value in (("first_sequence", 2), ("last_sequence", 1), ("last_sequence", 0)):
            with self.subTest(field=field, value=value):
                self.reject(lambda r, s: coverage(r).update({field: value}), "watermark")

    def test_epoch_inventory_and_scope_mismatch_refuse(self):
        self.reject(lambda r, s: coverage(r).update(enforcement_epoch="8"), "telemetry_scope")
        self.reject(lambda r, s: coverage(r).update(source_inventory_sha256="0" * 64),
                    "telemetry_scope")
        # The attempt epoch is cross-checked by the resource graph before telemetry.
        self.reject(lambda r, s: r["attempt"].update(enforcement_epoch="8"))

    def test_duplicate_alias_sequence_and_receipt_replay_refuse(self):
        self.reject(lambda r, s: edit(r, ACK1, operation_id=ACK0))
        self.reject(lambda r, s: effect_of(r, EDIT).update(sequence=99), "sequence_gap")
        self.reject(lambda r, s: effect_of(r, EDIT).update(sequence=1), "sequence_gap")
        self.reject(lambda r, s: coverage(r)["wire_receipt_refs"].append(
            coverage(r)["wire_receipt_refs"][0]), "reference_duplicate")
        self.reject(lambda r, s: coverage(r)["transaction_receipt_refs"].append(
            coverage(r)["transaction_receipt_refs"][0]), "reference_duplicate")

        def alias_wire(receipt, records):
            records["wire-alias"] = copy.deepcopy(records["wire-" + ACK0])
            records["wire-alias"]["ref"] = "wire-alias"
            coverage(receipt)["wire_receipt_refs"].append("wire-alias")
        self.reject(alias_wire, "wire_inventory")

        def alias_platform(receipt, records):
            records["platform-alias"] = copy.deepcopy(records["platform-" + FEED])
            records["platform-alias"]["ref"] = "platform-alias"
            coverage(receipt)["platform_receipt_refs"].append("platform-alias")
        self.reject(alias_platform, "platform_replay")


class Observer(Telemetry):
    def test_observer_job_must_span_entire_window(self):
        for field, value in (("started_at", at("00:00:10")), ("ended_at", at("00:23:59"))):
            with self.subTest(field=field):
                self.reject(lambda r, s: s["job-observer"]["data"].update({field: value}),
                            "telemetry_observer_window")

    def test_observer_must_be_external_independent_and_same_principal(self):
        # A non-external-observer job cannot supply full-window telemetry.
        self.reject(lambda r, s: set_observer(r, s, "resource-observer"),
                    "telemetry_observer_window")
        verifier = principal("verifier")

        def reuse_verifier(receipt, records):
            for ref in ("observer-ref", "producer-observer"):
                records[ref]["data"]["principal"] = copy.deepcopy(verifier)
        self.reject(reuse_verifier, "telemetry_independence|resource_observer", repin=False)
        executor = principal("executor")

        def reuse_executor(receipt, records):
            for ref in ("observer-ref", "producer-observer"):
                records[ref]["data"]["principal"] = copy.deepcopy(executor)
        self.reject(reuse_executor, "observer_independence|telemetry_independence", repin=False)

    def test_each_receipt_must_name_the_coverage_observer_and_stay_in_window(self):
        for kind, ref, code in (
                ("wire_attempt", "wire-" + ACK0, "wire_binding"),
                ("platform_effect", "platform-" + FEED, "telemetry_observation"),
                ("db_transaction", "transaction-" + DELIVERY, "telemetry_observation")):
            with self.subTest(kind=kind):
                self.reject(lambda r, s: s[ref]["data"].update(observer_ref="resource-observer"),
                            code)
        self.reject(lambda r, s: s["platform-" + FEED]["data"].update(observed_at=at("00:24:01")),
                    "telemetry_observation")
        self.reject(lambda r, s: s["transaction-" + DELIVERY]["data"].update(
            committed_at=at("00:24:01")), "telemetry_observation")


    def test_resource_controller_cannot_observe_own_effects(self):
        # The resource-state verifier stays separate; only the TELEMETRY observer
        # and its external producer are moved onto the authority controller.
        controller = principal("controller")

        def own_effects(receipt, records):
            for ref in ("observer-ref", "producer-observer"):
                records[ref]["data"]["principal"] = copy.deepcopy(controller)
            self.assertNotEqual(controller, records["resource-observer"]["data"]["principal"])
            self.assertEqual(controller, records["resource-owner"]["data"]["principal"])
        self.reject(own_effects, "telemetry_independence", repin=False)

    def test_budgeted_rsvp_read_can_share_atomic_insert_transaction(self):
        def with_read(receipt, records):
            operation = add_rsvp_read(receipt, records)
            self.assertEqual(3, len(transaction(records, RSVP_TX)["effects"]))
            self.assertEqual(operation, transaction(records, RSVP_TX)["effects"][-1]["operation_id"])
        receipt, records, now = telemetry_fixture()
        with_read(receipt, records)
        index = evidence_index(records)
        self.assertIsNone(check_telemetry_claims(receipt, index, now))
        self.assertIsNone(check_authority_graph_claims(receipt, index, now))
        index.require_all_used()
        # An unrelated WRITE in the pair's transaction must still refuse.
        self.reject(lambda r, s: (with_read(r, s), None), "rsvp_atomic_audit", repin=False,
                    after=lambda r, s: transaction(s, RSVP_TX)["effects"].append(
                        binding(r, effect_of(r, DELIVERY))))
        # A read outside the pair's transaction is a separate valid transaction
        # only if it is itself bound; an unbound read is a missing inventory.
        def unbound(receipt, records):
            with_read(receipt, records)
            transaction(records, RSVP_TX)["effects"].pop()
        self.reject(unbound, "transaction_inventory", repin=False)


class Wire(Telemetry):
    def test_wire_receipt_binds_exact_effect_identity_time_and_outcome(self):
        ref = "wire-" + ACK0
        for name, changes in (
                ("digest", {"effect_sha256": "0" * 64}),
                ("identity", {"resource_identity_sha256": "0" * 64}),
                ("time", {"observed_at": at("00:08:01")}),
                ("other_operation", {"operation_id": ACK1}),
                ("blocked", {"outcome": "blocked"}), ("failed", {"outcome": "failed"}),
                ("unknown", {"outcome": "unknown"})):
            with self.subTest(name=name):
                self.reject(lambda r, s: None, "wire_binding", after=lambda r, s: s[ref][
                    "data"].update(changes))

    def test_wire_receipt_for_database_or_unknown_operation_refuses(self):
        for operation in (DELIVERY, "ghost-operation"):
            with self.subTest(operation=operation):
                self.reject(lambda r, s: None, "wire_operation", after=lambda r, s: s[
                    "wire-" + ACK0]["data"].update(operation_id=operation))

    def test_stale_effect_digest_is_refused_not_repinned(self):
        # Both ledgers agree, but the wire digest was not refreshed: still bound.
        self.reject(lambda r, s: edit(r, IDENT, payload_sha256="b" * 64), "wire_binding",
                    repin=False)

    def test_wire_and_platform_only_effects_carry_no_foreign_receipts(self):
        self.reject(lambda r, s: edit(r, IDENT, db_transition_refs=["transition-x"]),
                    "wire_transition")
        self.reject(lambda r, s: edit(r, IDENT, platform_id_refs=["777"]),
                    "unplanned_platform_identity")
        self.reject(lambda r, s: edit(r, IDENT, nonce_ref=["nonce-x"]), "unplanned_nonce")

        def ack_nonce(receipt, records):
            edit(receipt, ACK0, nonce_ref=["nonce-x"])
            find(records, "platform_effect", ACK0)["nonce_refs"] = ["nonce-x"]
        self.reject(ack_nonce, "unplanned_nonce")

        def feed_nonce(receipt, records):
            edit(receipt, FEED, nonce_ref=["nonce-x"])
            find(records, "platform_effect", FEED)["nonce_refs"] = ["nonce-x"]
        self.reject(feed_nonce, "feed_nonce")

        def feed_repeat(receipt, records):
            nonce = receipt["fixtures"]["nonce_ref"]
            edit(receipt, FEED, nonce_ref=[nonce, nonce])
            find(records, "platform_effect", FEED)["nonce_refs"] = [nonce, nonce]
        self.reject(feed_repeat, "reference_duplicate")


class Platform(Telemetry):
    def test_platform_receipt_binds_exact_content_recipient_nonce_and_ids(self):
        for ref, name, changes in (
                (ACK0, "method", {"method": "interaction_edit"}),
                (ACK0, "payload", {"payload_sha256": "b" * 64}),
                (ACK0, "recipient", {"resource_identity_sha256": "0" * 64}),
                (ACK0, "ids_changed", {"platform_ids": ["1"]}),
                (ACK0, "ids_extra", {"platform_ids": ["9001", "9999"]}),
                (ACK0, "ids_empty", {"platform_ids": []}),
                (ACK0, "unknown", {"outcome": "unknown"}),
                (FEED, "nonce_missing", {"nonce_refs": []}),
                (FEED, "nonce_wrong", {"nonce_refs": ["nonce-x"]}),
                (FEED, "operation", {"operation_id": EDIT}),
                (EDIT, "ids_swapped", {"platform_ids": ["9001"]})):
            with self.subTest(ref=ref, name=name):
                self.reject(lambda r, s: None, "array_size" if name == "ids_empty" else
                            "platform_binding", after=lambda r, s: s[
                                "platform-" + ref]["data"].update(changes))

    def test_platform_receipt_for_wire_only_or_unknown_operation_refuses(self):
        for operation in (IDENT, DELIVERY, "ghost-operation"):
            with self.subTest(operation=operation):
                self.reject(lambda r, s: None, "platform_operation", after=lambda r, s: s[
                    "platform-" + ACK0]["data"].update(operation_id=operation))

    def test_same_method_platform_identifier_replay_and_duplicates_refuse(self):
        def replay(receipt, records):
            identifier = effect_of(receipt, ACK0)["platform_id_refs"]
            edit(receipt, ACK1, platform_id_refs=identifier)
            find(records, "platform_effect", ACK1)["platform_ids"] = list(identifier)
        self.reject(replay, "platform_replay")

        def twice(receipt, records):
            identifier = effect_of(receipt, ACK0)["platform_id_refs"][0]
            edit(receipt, ACK0, platform_id_refs=[identifier, identifier])
            find(records, "platform_effect", ACK0)["platform_ids"] = [identifier, identifier]
        self.reject(twice, "reference_duplicate")

        def two_ids(receipt, records):
            ids = [effect_of(receipt, ACK0)["platform_id_refs"][0], "9999"]
            edit(receipt, ACK0, platform_id_refs=ids)
            find(records, "platform_effect", ACK0)["platform_ids"] = list(ids)
        self.reject(two_ids, "platform_count")

    def test_platform_observation_cannot_precede_or_leave_effect_window(self):
        self.reject(lambda r, s: None, "platform_order", after=lambda r, s: s[
            "platform-" + FEED]["data"].update(observed_at=at("00:07:59")))
        self.reject(lambda r, s: None, "telemetry_observation", after=lambda r, s: s[
            "platform-" + FEED]["data"].update(observed_at=at("00:24:01")))
        self.reject(lambda r, s: None, "telemetry_observation", after=lambda r, s: s[
            "platform-" + FEED]["data"].update(observed_at=at("23:59:59").replace(
                "2026-01-01", "2025-12-31")))


class Database(Telemetry):
    def test_every_database_effect_needs_exactly_one_committed_transaction(self):
        self.reject(lambda r, s: coverage(r)["transaction_receipt_refs"].remove(
            "transaction-" + DELIVERY), "transaction_inventory")
        self.reject(lambda r, s: coverage(r)["transaction_receipt_refs"].append(
            coverage(r)["transaction_receipt_refs"][0]), "reference_duplicate")
        self.reject(lambda r, s: s["transaction-" + DELIVERY]["data"].update(
            transaction_id=s["transaction-" + CAS]["data"]["transaction_id"]),
            "reference_duplicate")
        for outcome in ("rolled_back", "unknown"):
            with self.subTest(outcome=outcome):
                self.reject(lambda r, s: s["transaction-" + DELIVERY]["data"].update(
                    outcome=outcome), "uncertain_transaction")

    def test_transaction_binds_effect_digest_time_and_database_identity(self):
        ref = "transaction-" + DELIVERY
        self.reject(lambda r, s: None, "transaction_binding", after=lambda r, s: s[ref][
            "data"]["effects"][0].update(effect_sha256="0" * 64))
        self.reject(lambda r, s: None, "transaction_binding", after=lambda r, s: s[ref][
            "data"]["effects"][0].update(resource_identity_sha256="0" * 64))
        self.reject(lambda r, s: s[ref]["data"].update(committed_at=at("00:08:01")),
                    "transaction_binding")
        self.reject(lambda r, s: edit(r, DELIVERY, platform_id_refs=["777"]),
                    "transaction_binding")
        # A stale digest after an effect change stays bound, not silently repinned.
        self.reject(lambda r, s: edit(r, DELIVERY, payload_sha256="b" * 64),
                    "transaction_binding", repin=False)

    def test_transaction_members_must_be_database_effects_without_aliases(self):
        for operation in ("ghost-operation", IDENT):
            with self.subTest(operation=operation):
                self.reject(lambda r, s: None, "transaction_operation",
                            after=lambda r, s: s["transaction-" + DELIVERY]["data"][
                                "effects"][0].update(operation_id=operation))

        def alias(receipt, records):
            clone = copy.deepcopy(records["transaction-" + DELIVERY])
            clone["ref"] = "transaction-alias"
            clone["data"]["transaction_id"] = "0000abcd-0000-4000-8000-000000000000"
            records["transaction-alias"] = clone
            coverage(receipt)["transaction_receipt_refs"].append("transaction-alias")
        self.reject(alias, "transition_binding|reference_duplicate|transaction_inventory")

    def test_transition_must_match_owner_key_count_method_and_transaction(self):
        ref = "transition-" + CAS
        for name, claim in (
                ("owner", {"owner": "other-owner"}), ("key", {"key_sha256": "0" * 64}),
                ("operation", {"operation_id": DELIVERY}),
                ("resource", {"resource_ref": "delivery"}),
                ("state", {"actual_after_state_ref": "before-setting"}),
                ("expected_two", {"expected_count": 2}), ("actual_two", {"actual_count": 2}),
                ("actual_zero", {"actual_count": 0}),
                ("expected_zero", {"expected_count": 0}),
                ("time", {"observed_at": at("00:08:01")}),
                ("conflict", {"outcome": "conflict"}), ("unknown", {"outcome": "unknown"}),
                ("failed", {"outcome": "failed"})):
            with self.subTest(name=name):
                self.reject(lambda r, s: None, "transition_binding", after=lambda r, s: s[ref][
                    "data"]["claim"].update(claim))
        for name, top in (("method", {"method": "db_delete_owned"}),
                          ("transaction", {"transaction_ref": "transaction-" + DELIVERY})):
            with self.subTest(name=name):
                self.reject(lambda r, s: None, "transition_binding",
                            after=lambda r, s: s[ref]["data"].update(top))

    def test_transition_inventory_per_effect_and_per_transaction(self):
        self.reject(lambda r, s: edit(r, CAS, db_transition_refs=[]), "missing_transition_receipts")
        self.reject(lambda r, s: edit(r, CAS, db_transition_refs=[
            "transition-" + CAS, "transition-extra"]), "transition_inventory")
        tx = "transaction-" + CAS
        self.reject(lambda r, s: None, "transaction_transitions",
                    after=lambda r, s: s[tx]["data"].update(transition_refs=[]))
        self.reject(lambda r, s: None, "transaction_transitions", after=lambda r, s: s[tx][
            "data"]["transition_refs"].append("transition-extra"))
        self.reject(lambda r, s: None, "reference_duplicate", after=lambda r, s: s[tx][
            "data"]["transition_refs"].append("transition-" + CAS))
        self.reject(lambda r, s: None, "transaction_transitions", after=lambda r, s: s[tx][
            "data"].update(transition_refs=["transition-" + DELIVERY]))


class AtomicRsvp(Telemetry):
    def test_rsvp_and_audit_split_across_transactions_refuses(self):
        def split(receipt, records):
            pair = transaction(records, RSVP_TX)
            audit = pair["effects"].pop()
            pair["transition_refs"].pop()
            records["transaction-audit"] = copy.deepcopy(records[RSVP_TX])
            records["transaction-audit"]["ref"] = "transaction-audit"
            records["transaction-audit"]["data"].update(
                transaction_id="0000abcd-0000-4000-8000-000000000000", effects=[audit],
                transition_refs=["transition-" + AUDIT])
            records["transition-" + AUDIT]["data"]["transaction_ref"] = "transaction-audit"
            coverage(receipt)["transaction_receipt_refs"].append("transaction-audit")
        self.reject(split, "rsvp_atomic_audit")

    def test_rsvp_audit_missing_or_mixed_with_other_effect_refuses(self):
        def drop_audit(receipt, records):
            pair = transaction(records, RSVP_TX)
            pair["effects"].pop()
            pair["transition_refs"].pop()
        self.reject(drop_audit, "rsvp_atomic_audit")
        self.reject(lambda r, s: None, "rsvp_atomic_audit", after=lambda r, s: transaction(
            s, RSVP_TX)["effects"].append(binding(r, effect_of(r, DELIVERY))))
        self.reject(lambda r, s: None, "rsvp_atomic_audit", after=lambda r, s: transaction(
            s, "transaction-" + DELIVERY)["effects"].append(binding(r, effect_of(r, RSVP))))

    def test_rsvp_pair_in_uncommitted_transaction_refuses(self):
        for outcome in ("rolled_back", "unknown"):
            with self.subTest(outcome=outcome):
                self.reject(lambda r, s: transaction(s, RSVP_TX).update(outcome=outcome),
                            "uncertain_transaction")


class Budgets(Telemetry):
    def test_method_cap_is_enforced_beyond_resource_aggregate(self):
        def shift(receipt, records):
            budgets = records["runtime-resource-plan"]["data"]["operation_budgets"]
            by_method = {x["method"]: x for x in budgets if x["resource_ref"] == "setting"}
            by_method["db_cas"].update(expected_count=1, maximum_count=1)
            by_method["db_insert"].update(expected_count=2, maximum_count=2)
            repin_resources(receipt, records)
        self.reject(shift, "budget")

        def retype(receipt, records):
            # Aggregate for the channel stays three, the per-method counts do not.
            edit(receipt, ACK1, method="interaction_edit")
            find(records, "platform_effect", ACK1)["method"] = "interaction_edit"
        self.reject(retype, "budget")

        def loosen(receipt, records):
            budgets = records["runtime-resource-plan"]["data"]["operation_budgets"]
            next(x for x in budgets if x["resource_ref"] == "feed-channel" and
                 x["method"] == "feed_post").update(maximum_count=2)
            repin_resources(receipt, records)
        self.reject(loosen, "budget|operation_budget")


class Adverse(Telemetry):
    def test_blocked_failed_unknown_or_unaccepted_effects_refuse(self):
        for operation in (IDENT, ACK0, CAS):
            for outcome in ("blocked", "failed", "unknown"):
                with self.subTest(operation=operation, outcome=outcome):
                    self.reject(lambda r, s: edit(r, operation, outcome=outcome, accepted_count=0),
                                "uncertain_effect")
            with self.subTest(operation=operation, name="unaccepted"):
                self.reject(lambda r, s: edit(r, operation, accepted_count=0), "uncertain_effect")

    def test_unplanned_method_resource_or_time_effect_refuses(self):
        self.reject(lambda r, s: edit(r, IDENT, method="member_get"), "unplanned_effect|budget")
        self.reject(lambda r, s: edit(r, IDENT, method="db_read"), "unplanned_effect|budget")
        self.reject(lambda r, s: edit(r, IDENT, resource_ref="ghost-resource"), "unplanned_effect",
                    repin=False)
        self.reject(lambda r, s: edit(r, IDENT, observed_at=at("00:25:00")), "provenance_time")
        self.reject(lambda r, s: edit(r, IDENT, observed_at=at("23:59:59").replace(
            "2026-01-01", "2025-12-31")), "provenance_time")

    def test_expected_and_actual_ledgers_must_agree_exactly(self):
        self.reject(lambda r, s: effect_of(r, FEED).update(payload_sha256="b" * 64),
                    "effect_multiset")
        self.reject(lambda r, s: effect_of(r, FEED).update(platform_id_refs=["1"]),
                    "effect_multiset")
        self.reject(lambda r, s: effect_of(r, CAS).update(db_transition_refs=["transition-x"]),
                    "effect_multiset")


if __name__ == "__main__":
    unittest.main()
