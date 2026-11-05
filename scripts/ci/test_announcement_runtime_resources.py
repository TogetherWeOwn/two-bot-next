"""Hermetic resource-plan adversaries; not authentication or staging evidence."""

import copy
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from announcement_resource_fixtures import resource_fixture, repin_resources
from announcement_provenance_fixtures import at, evidence_index
from announcement_runtime_authority import check_authority_graph_claims
from announcement_runtime_evidence import RESOURCE_RECORDS, canonical_digest
from announcement_runtime_policy import runtime_preflight
from announcement_runtime_provenance import check_provenance_claims
from announcement_runtime_resources import check_resource_graph_claims
from announcement_runtime_schema import Refusal


def entry(receipt, ref):
    return next(x for x in receipt["resources"]["entries"] if x["ref"] == ref)


def budget(records, ref, method):
    return next(x for x in records["runtime-resource-plan"]["data"]["operation_budgets"]
                if x["resource_ref"] == ref and x["method"] == method)


def change_identity(receipt, records, ref, **changes):
    # Preserve copied claims/hash equality so tests reach semantic predicates.
    identity = entry(receipt, ref)["identity"]
    identity.update(changes)
    binding = next(x for x in records["runtime-identity-map"]["data"]["bindings"]
                   if x["resource_ref"] == ref)
    binding["identity"] = copy.deepcopy(identity)
    for key in ("isolation-" + ref, "before-" + ref, "after-" + ref):
        records[key]["data"]["identity_sha256"] = canonical_digest(identity)


class ResourceGraph(unittest.TestCase):
    def check(self, receipt, records, now):
        return check_resource_graph_claims(receipt, evidence_index(records), now)

    def reject(self, mutate, code=None, *, repin=False):
        receipt, records, now = resource_fixture()
        mutate(receipt, records)
        if repin:
            repin_resources(receipt, records)
        with self.assertRaisesRegex(Refusal, code or "announcement evidence refused"):
            self.check(receipt, records, now)

    def test_consistent_resource_claims_return_no_authority(self):
        receipt, records, now = resource_fixture()
        index = evidence_index(records)
        self.assertIsNone(check_resource_graph_claims(receipt, index, now))
        self.assertIsNone(check_authority_graph_claims(receipt, index, now))
        self.assertIsNone(check_provenance_claims(receipt, index, now))
        index.require_all_used()
        with self.assertRaisesRegex(Refusal, "unsupported"):
            runtime_preflight()

    def test_all_resource_sidecar_kinds_are_closed_and_nonempty(self):
        _, records, _ = resource_fixture()
        examples = {r["kind"]: ref for ref, r in records.items() if r["kind"] in RESOURCE_RECORDS}
        self.assertEqual(set(examples), set(RESOURCE_RECORDS))
        for kind, ref in examples.items():
            for data in ({}, {**records[ref]["data"], "extra": True}, None):
                with self.subTest(kind=kind, data=type(data)):
                    self.reject(lambda r, s: s[ref].update(data=copy.deepcopy(data)))

    def test_missing_wrong_kind_or_other_attempt_always_refuses(self):
        for ref in ("runtime-resource-plan", "runtime-identity-map", "resource-owner",
                    "resource-observer", "isolation-app", "before-rsvp", "after-setting", "item-ref"):
            with self.subTest(ref=ref):
                self.reject(lambda r, s: s.pop(ref), "unresolved_reference")
                self.reject(lambda r, s: s[ref].update(kind="document"))
                self.reject(lambda r, s: s[ref]["run"].update(attempt=2), "evidence_attempt")

    def test_content_not_just_document_names_is_pinned(self):
        self.reject(lambda r, s: s["runtime-resource-plan"]["data"]["slots"].update(
            feed_channel_ref="control-channel"), "resource_content")
        self.reject(lambda r, s: s["runtime-identity-map"]["data"]["bindings"][0]["identity"].update(
            object_id="101"), "resource_content")
        self.reject(lambda r, s: r["resources"]["plan"].update(sha256="0" * 64), "reference_binding")

    def test_content_pins_must_match_receipt_entries_and_states(self):
        self.reject(lambda r, s: entry(r, "feed-channel").update(maximum_count=99), "resource_plan")
        self.reject(lambda r, s: r["resources"]["states"][0].update(version="2"), "resource_plan")
        self.reject(lambda r, s: r["authority"]["resource_plan"].update(sha256="0" * 64),
                    "resource_plan")

    def test_plan_inventory_and_identity_map_do_not_accept_aliased_resources(self):
        self.reject(lambda r, s: r["resources"]["entries"].append(copy.deepcopy(
            r["resources"]["entries"][0])), "resource_inventory", repin=True)
        self.reject(lambda r, s: s["runtime-identity-map"]["data"]["bindings"].pop(),
                    "identity_map", repin=True)
        self.reject(lambda r, s: s["runtime-identity-map"]["data"]["bindings"].append(copy.deepcopy(
            s["runtime-identity-map"]["data"]["bindings"][0])), "identity_map", repin=True)
        self.reject(lambda r, s: change_identity(r, s, "control-channel", object_id="500"),
                    "resource_alias", repin=True)

    def test_exact_slots_cannot_be_missing_swapped_or_shared(self):
        for changes in ({"feed_channel_ref": "not-listed"}, {"feed_ref": "rsvp"},
                        {"actor_ref": "event"}, {"control_channel_ref": "feed-channel"}):
            with self.subTest(changes=changes):
                self.reject(lambda r, s: s["runtime-resource-plan"]["data"]["slots"].update(changes),
                            "resource_slots", repin=True)
        self.reject(lambda r, s: r["fixtures"].update(feed_ref="delivery"), "fixture_binding")

    def test_one_application_and_one_guild_without_discord_object_aliases(self):
        for ref, changes in (("app", {"application_id": "999"}),
                ("actor", {"guild_id": "999"}), ("event", {"application_id": "999"}),
                ("feed-channel", {"object_type": "message"}),
                ("event", {"object_id": "300"})):
            with self.subTest(ref=ref, changes=changes):
                self.reject(lambda r, s: change_identity(r, s, ref, **changes),
                            "discord_identity", repin=True)

    def test_database_table_role_endpoint_grants_and_triggers_are_bound(self):
        for field, value in (("table", "event_rsvps"), ("role_ref", "different-role"),
                ("database_ref", "other-database"), ("endpoint_identity_sha256", "0" * 64),
                ("grants_sha256", "0" * 64), ("trigger_inventory_sha256", "0" * 64)):
            with self.subTest(field=field):
                self.reject(lambda r, s: change_identity(r, s, "delivery", **{field: value}),
                            "database_", repin=True)

    def test_identity_binding_cannot_point_to_another_resource(self):
        self.reject(lambda r, s: s["runtime-identity-map"]["data"]["bindings"][0].update(
            owner_ref="another-owner"), "identity_map", repin=True)
        self.reject(lambda r, s: s["runtime-identity-map"]["data"]["bindings"][0]["identity"].update(
            guild_id="900"), "identity_map", repin=True)

    def test_trusted_owner_is_bound_to_execution_nonce_and_controller(self):
        for changes in ({"execution_ref": "previous-execution"}, {"nonce_ref": "old-nonce"}):
            with self.subTest(changes=changes):
                self.reject(lambda r, s: s["resource-owner"]["data"].update(changes), "resource_owner")
        self.reject(lambda r, s: s["resource-owner"]["data"].update(
            principal=r["authority"]["executor"]), "resource_owner")

    def test_resource_observer_is_separately_provisioned_verifier_claim(self):
        for changes in ({"provisioning_ref": "controller-provisioning"},
                        {"principal": {"ref": "author-ref", "subject_id": "1" * 8 +
                            "-0000-4000-8000-000000000000", "identity_root_ref": "native-mapping-ref"}}):
            with self.subTest(changes=changes):
                self.reject(lambda r, s: s["resource-observer"]["data"].update(changes),
                            "resource_observer")
        self.reject(lambda r, s: s["verifier-consumer"]["data"].update(
            provisioning_ref="controller-provisioning"), "resource_observer")

    def test_raw_credentials_application_flags_unknowns_and_production_membership_refuse(self):
        for changes in ({"boundary": "application_only"}, {"boundary": "unknown"},
                        {"credential_exposure": "raw"}, {"credential_exposure": "unknown"},
                        {"production_memberships": ["900"]}, {"resource_epoch": "8"},
                        {"resource_ref": "rsvp"}, {"owner_ref": "other-owner"},
                        {"identity_sha256": "0" * 64}):
            with self.subTest(changes=changes):
                self.reject(lambda r, s: s["isolation-app"]["data"].update(changes),
                            "resource_isolation")

    def test_isolation_is_observed_before_rollout_not_later(self):
        for clock in ("00:03:01",):
            self.reject(lambda r, s: s["isolation-app"]["data"].update(observed_at=at(clock)),
                        "resource_baseline")
        receipt, records, now = resource_fixture()
        records["isolation-app"]["data"]["observed_at"] = receipt["rollout"]["started_at"]
        self.check(receipt, records, now)

    def test_isolation_records_cannot_be_shared_across_resources(self):
        self.reject(lambda r, s: s["runtime-identity-map"]["data"]["bindings"][1].update(
            isolation_ref="isolation-app"), "resource_isolation", repin=True)

    def test_state_refs_are_unique_complete_and_resolve_exact_claims(self):
        self.reject(lambda r, s: r["resources"]["states"].append(copy.deepcopy(
            r["resources"]["states"][0])), "state_inventory", repin=True)
        self.reject(lambda r, s: r["resources"]["states"].pop(), "state_inventory", repin=True)
        self.reject(lambda r, s: entry(r, "actor").update(before_state_ref="before-app"),
                    "state_inventory", repin=True)
        self.reject(lambda r, s: s["before-app"]["data"]["claim"].update(version="2"), "state_binding")
        self.reject(lambda r, s: s["before-app"]["data"].update(resource_ref="actor"), "state_binding")
        self.reject(lambda r, s: s["after-app"]["data"].update(identity_sha256="0" * 64), "state_binding")

    def test_before_and_after_states_have_separate_same_attempt_windows(self):
        for ref, clock in (("before-app", "00:03:01"), ("after-app", "00:03:59"),
                           ("after-app", "00:24:01")):
            with self.subTest(ref=ref, clock=clock):
                self.reject(lambda r, s: s[ref]["data"].update(observed_at=at(clock)), "state_time")

    def test_preexisting_delivery_rsvp_and_audit_are_not_adopted(self):
        for ref in ("delivery", "rsvp", "rsvp-audit"):
            def mutate(r, s):
                state = next(x for x in r["resources"]["states"] if x["ref"] == "before-" + ref)
                state.update(presence="present", version="1", value_sha256="0" * 64)
                s["before-" + ref]["data"]["claim"] = copy.deepcopy(state)
            with self.subTest(ref=ref):
                self.reject(mutate, "fixture_conflict", repin=True)

    def test_item_source_content_nonce_and_fixture_refs_are_exact(self):
        for changes in ({"source_ref": "other-source"}, {"feed_ref": "delivery"},
                        {"nonce_ref": "old-nonce"}, {"item_ref": "another-item"},
                        {"source_content_sha256": "0" * 64}, {"item_content_sha256": "0" * 64}):
            with self.subTest(changes=changes):
                self.reject(lambda r, s: s["item-ref"]["data"].update(changes), "fixture_binding")
        self.reject(lambda r, s: change_identity(r, s, "source", item_ref="other-item"),
                    "fixture_binding", repin=True)

    def test_resources_and_plan_cannot_outlive_lease_or_exclude_cleanup(self):
        self.reject(lambda r, s: s["runtime-resource-plan"]["data"].update(
            expires_at=at("00:40:01")), "resource_window", repin=True)
        self.reject(lambda r, s: s["runtime-resource-plan"]["data"].update(
            not_before=at("00:00:01")), "resource_window", repin=True)
        self.reject(lambda r, s: entry(r, "actor").update(expires_at=at("00:24:00")),
                    "resource_window", repin=True)

    def test_method_ceiling_is_not_a_caller_supplied_generic_allowlist(self):
        self.reject(lambda r, s: entry(r, "rsvp").update(allowed_operations=["feed_post"]),
                    "resource_methods", repin=True)
        self.reject(lambda r, s: entry(r, "source")["allowed_operations"].append("source_get"),
                    "resource_methods", repin=True)
        for method in ("dm_send", "http_request", "db_sql", "global_commands_replace"):
            with self.subTest(method=method):
                self.reject(lambda r, s: entry(r, "app").update(allowed_operations=[method]), repin=True)

    def test_per_method_budgets_are_unique_complete_and_not_just_aggregate(self):
        self.reject(lambda r, s: s["runtime-resource-plan"]["data"]["operation_budgets"].pop(),
                    "operation_budget", repin=True)
        self.reject(lambda r, s: s["runtime-resource-plan"]["data"]["operation_budgets"].append(
            copy.deepcopy(s["runtime-resource-plan"]["data"]["operation_budgets"][0])),
            "operation_budget", repin=True)
        def move(r, s):
            budget(s, "feed-channel", "feed_post").update(expected_count=0, maximum_count=0)
            budget(s, "feed-channel", "history_get").update(expected_count=2, maximum_count=2)
        self.reject(move, "profile_budget", repin=True)
        self.reject(lambda r, s: entry(r, "app").update(maximum_count=2),
                    "operation_budget", repin=True)

    def test_total_cumulative_budget_and_expected_maximum_order_refuse(self):
        self.reject(lambda r, s: budget(s, "source", "source_get").update(maximum_count=513),
                    "budget", repin=True)
        self.reject(lambda r, s: budget(s, "source", "source_get").update(maximum_count=0),
                    "budget", repin=True)
        self.reject(lambda r, s: budget(s, "source", "source_get").update(maximum_count=512),
                    "budget", repin=True)

    def test_positive_profile_counts_cannot_be_inflated(self):
        for ref, method in (("feed-channel", "feed_post"), ("rsvp", "db_insert"),
                            ("rsvp-audit", "db_insert"), ("delivery", "db_insert"),
                            ("delivery", "db_cas"), ("setting", "db_cas")):
            def mutate(r, s):
                b = budget(s, ref, method)
                b["expected_count"] += 1
                b["maximum_count"] += 1
                entry(r, ref)["expected_count"] += 1
                entry(r, ref)["maximum_count"] += 1
            with self.subTest(ref=ref, method=method):
                self.reject(mutate, "profile_budget", repin=True)

    def test_each_actual_and_expected_method_count_must_match_plan(self):
        for ledger in ("actual_effects", "expected_effects"):
            with self.subTest(ledger=ledger):
                self.reject(lambda r, s: r["observations"][ledger].pop(), "budget")
                self.reject(lambda r, s: r["observations"][ledger][0].update(resource_ref="not-planned"),
                            "unplanned_effect")
                self.reject(lambda r, s: r["observations"][ledger][0].update(method="gateway_read"),
                            "unplanned_effect")

    def test_zero_expected_setting_transitions_cannot_hide_behind_maximum(self):
        def mutate(r, s):
            budget(s, "setting", "db_cas")["expected_count"] = 0
            entry(r, "setting")["expected_count"] -= 2
            for ledger in ("actual_effects", "expected_effects"):
                r["observations"][ledger] = [x for x in r["observations"][ledger]
                    if not (x["resource_ref"] == "setting" and x["method"] == "db_cas")]
        self.reject(mutate, "profile_budget", repin=True)

    def test_resource_observation_job_and_producer_must_be_independent_and_current(self):
        for changes in ({"source": "candidate"}, {"head": "0" * 40},
                        {"conclusion": "failure"}, {"started_at": at("00:02:00")}):
            with self.subTest(changes=changes):
                self.reject(lambda r, s: s["job-resource-observer"]["data"].update(changes),
                            "resource_observer")
        self.reject(lambda r, s: s["producer-resource-observer"]["data"].update(
            principal=r["authority"]["caller"]), "resource_observer")
        self.reject(lambda r, s: r["attempt"]["job_refs"].remove("job-resource-observer"),
                    "resource_observer")

    def test_existing_discord_and_source_resources_cannot_have_absent_baselines(self):
        def mutate(r, s):
            state = next(x for x in r["resources"]["states"] if x["ref"] == "before-actor")
            state.clear()
            state.update(ref="before-actor", presence="absent")
            s["before-actor"]["data"]["claim"] = copy.deepcopy(state)
        self.reject(mutate, "fixture_conflict", repin=True)

    def test_synthetic_dependencies_are_finite_disjoint_and_accounted(self):
        receipt, records, now = resource_fixture(synthetic=True)
        index = evidence_index(records)
        check_resource_graph_claims(receipt, index, now)
        check_authority_graph_claims(receipt, index, now)
        check_provenance_claims(receipt, index, now)
        index.require_all_used()
        for changes in ({"resource_refs": ["actor"]}, {"resource_refs": ["not-planned"]},
                        {"resource_refs": ["synthetic"]},
                        {"resource_refs": ["synthetic-actor", "synthetic-actor"]},
                        {"actor_ref": "event"}, {"resource_refs": ["synthetic-channel"]}):
            r, s, now = resource_fixture(synthetic=True)
            change_identity(r, s, "synthetic", **changes)
            repin_resources(r, s)
            with self.subTest(changes=changes), self.assertRaisesRegex(Refusal, "synthetic_resources"):
                self.check(r, s, now)
        r, s, now = resource_fixture(synthetic=True)
        change_identity(r, s, "synthetic", resource_refs=["synthetic-actor"])
        repin_resources(r, s)
        with self.assertRaisesRegex(Refusal, "extra_resource"):
            self.check(r, s, now)

    def test_diagnostics_never_include_rejected_values(self):
        receipt, records, now = resource_fixture()
        entry(receipt, "actor")["owner"] = "private-synthetic-sentinel"
        with self.assertRaises(Refusal) as raised:
            self.check(receipt, records, now)
        self.assertNotIn("private-synthetic-sentinel", str(raised.exception))


if __name__ == "__main__":
    unittest.main()
