"""Offline restricted input adversaries; never live consent or observations."""

import copy
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from announcement_input_fixtures import input_fixture, repin_inputs
from announcement_provenance_fixtures import at, evidence_index
from announcement_runtime_authority import check_authority_graph_claims
from announcement_runtime_evidence import INPUT_RECORDS, canonical_digest
from announcement_runtime_inputs import check_input_graph_claims
from announcement_runtime_policy import runtime_preflight
from announcement_runtime_provenance import check_provenance_claims
from announcement_resource_fixtures import repin_resources
from announcement_runtime_schema import Refusal


def data(records, ref="runtime-input-plan"):
    return records[ref]["data"]


class InputGraph(unittest.TestCase):
    def check(self, receipt, records, now):
        return check_input_graph_claims(receipt, evidence_index(records), now)

    def reject(self, mutate, code=None, *, repin=True):
        receipt, records, now = input_fixture()
        mutate(receipt, records)
        if repin:
            repin_inputs(receipt, records)
        with self.assertRaisesRegex(Refusal, code or "announcement evidence refused"):
            self.check(receipt, records, now)

    def test_consistent_claims_compose_but_never_admit_runtime(self):
        receipt, records, now = input_fixture()
        index = evidence_index(records)
        self.assertIsNone(check_input_graph_claims(receipt, index, now))
        self.assertIsNone(check_authority_graph_claims(receipt, index, now))
        self.assertIsNone(check_provenance_claims(receipt, index, now))
        index.require_all_used()
        with self.assertRaisesRegex(Refusal, "unsupported"):
            runtime_preflight()

    def test_each_input_kind_rejects_unknown_empty_and_null_objects(self):
        _, records, _ = input_fixture()
        examples = {r["kind"]: ref for ref, r in records.items() if r["kind"] in INPUT_RECORDS}
        self.assertEqual(set(examples), set(INPUT_RECORDS))
        for kind, ref in examples.items():
            for value in (None, {}, {**data(records, ref), "extra": True}):
                with self.subTest(kind=kind, value=type(value)):
                    self.reject(lambda r, s: s[ref].update(data=value), repin=False)

    def test_missing_wrong_type_or_other_attempt_input_refuses(self):
        for ref in ("runtime-input-plan", "key-feed", "key-delivery", "consent-ref",
                    "interaction-plan-ref"):
            with self.subTest(ref=ref):
                self.reject(lambda r, s: s.pop(ref), "unresolved_reference", repin=False)
                self.reject(lambda r, s: s[ref].update(kind="document"), repin=False)
                self.reject(lambda r, s: s[ref]["run"].update(attempt=2), "evidence_attempt", repin=False)

    def test_input_content_is_bound_to_resource_plan_not_just_names(self):
        self.reject(lambda r, s: data(s)["feed"].update(item_key="other-item"),
                    "input_content", repin=False)
        self.reject(lambda r, s: data(s, "consent-ref").update(actor_id="301"),
                    "input_content", repin=False)
        self.reject(lambda r, s: data(s, "key-rsvp").update(event_id="601"),
                    "input_content", repin=False)
        self.reject(lambda r, s: data(s, "interaction-plan-ref")["interactions"][0].update(
            request_sha256="0" * 64), "input_content", repin=False)

    def test_full_unique_literal_key_inventory_required(self):
        self.reject(lambda r, s: data(s)["keys"].pop(), "key_inventory")
        self.reject(lambda r, s: data(s)["keys"].append(copy.deepcopy(data(s)["keys"][0])),
                    "key_inventory")
        self.reject(lambda r, s: data(s)["keys"][1].update(resource_ref="not-listed"), "key_inventory")
        self.reject(lambda r, s: data(s)["keys"][1].update(key=copy.deepcopy(data(s)["keys"][0]["key"])),
                    "key_inventory")

    def test_equal_arbitrary_hashes_do_not_replace_literal_key_contents(self):
        self.reject(lambda r, s: data(s, "key-feed").update(id="other-relay"), "literal_key")
        self.reject(lambda r, s: data(s, "key-setting").update(guild_id="201"), "literal_key")

    def change_key(self, ref, changes, code):
        def mutate(receipt, records):
            data(records, "key-" + ref).update(changes)
            entry = next(x for x in receipt["resources"]["entries"] if x["ref"] == ref)
            entry["identity"]["key_sha256"] = canonical_digest(data(records, "key-" + ref))
            binding = next(x for x in data(records, "runtime-identity-map")["bindings"]
                           if x["resource_ref"] == ref)
            binding["identity"] = copy.deepcopy(entry["identity"])
            for state_ref in ("before-" + ref, "after-" + ref, "isolation-" + ref):
                data(records, state_ref)["identity_sha256"] = canonical_digest(entry["identity"])
        self.reject(mutate, code)

    def test_delivery_key_must_be_same_relay_and_same_item(self):
        self.change_key("delivery", {"feed_id": "other-relay"}, "delivery_key")
        self.change_key("delivery", {"item_key": "other-item"}, "delivery_key")

    def test_setting_and_rsvp_literal_keys_bind_exact_guild_event_actor(self):
        self.change_key("setting", {"guild_id": "201"}, "setting_key")
        for changes in ({"guild_id": "201"}, {"event_id": "601"}, {"user_id": "301"}):
            with self.subTest(changes=changes):
                self.change_key("rsvp", changes, "rsvp_key")

    def test_audit_primary_key_is_source_backed_rsvp_interaction_id(self):
        self.change_key("rsvp-audit", {"id": "rsvp:910"}, "rsvp_input")
        self.change_key("rsvp-audit", {"id": "checkin:909"}, "rsvp_input")
        self.reject(lambda r, s: data(s)["rsvp"].update(interaction_id="910"), "rsvp_input")
        self.reject(lambda r, s: data(s)["rsvp"].update(audit_ref="delivery"), "rsvp_input")

    def test_literal_key_fields_do_not_accept_wildcards_sql_or_foreign_tables(self):
        for changes in ({"id": "*"}, {"id": "all"}, {"id": "a/b"}, {"id": "x;DELETE"},
                        {"table": "guild_settings_revision"}, {"guild_id": "200"}):
            with self.subTest(changes=changes):
                self.reject(lambda r, s: data(s, "key-feed").update(changes))
        self.reject(lambda r, s: data(s, "key-setting").update(key="TWO_OTHER"))

    def test_feed_input_binds_source_destination_guild_fixture_and_nonce(self):
        for changes in ({"resource_ref": "delivery"}, {"guild_id": "201"}, {"channel_id": "400"},
                        {"source_ref": "not-listed"}, {"nonce_ref": "another-nonce"}):
            with self.subTest(changes=changes):
                self.reject(lambda r, s: data(s)["feed"].update(changes), "feed_input")

    def test_input_references_cannot_substitute_other_records(self):
        self.reject(lambda r, s: r["fixtures"].update(consent_ref="other-consent"), "input_reference")
        self.reject(lambda r, s: r["fixtures"].update(interaction_plan_ref="other-plan"),
                    "input_reference")

    def test_consent_binds_actor_app_guild_execution_and_one_shot_nonce(self):
        for changes in ({"actor_ref": "app"}, {"actor_id": "301"}, {"application_id": "101"},
                        {"guild_id": "201"}, {"execution_ref": "old-execution"},
                        {"nonce_ref": "old-nonce"}, {"outcome": "revoked"}, {"outcome": "unknown"}):
            with self.subTest(changes=changes):
                self.reject(lambda r, s: data(s, "consent-ref").update(changes), "test_consent")

    def test_consent_precedes_start_and_covers_full_window(self):
        for changes in ({"granted_at": at("00:00:01")}, {"not_before": at("00:00:01")},
                        {"expires_at": at("00:23:59")}, {"expires_at": at("00:24:00")}):
            with self.subTest(changes=changes):
                self.reject(lambda r, s: data(s, "consent-ref").update(changes), "consent_window")

    def test_consent_cases_are_exact_unique_and_nonempty(self):
        self.reject(lambda r, s: data(s, "consent-ref")["case_ids"].pop(), "consent_cases")
        self.reject(lambda r, s: data(s, "consent-ref")["case_ids"].append("other-case"), "consent_cases")
        self.reject(lambda r, s: data(s, "consent-ref")["case_ids"].append(
            data(s, "consent-ref")["case_ids"][0]), "consent_cases")

    def test_case_inventory_must_match_fixture_ids_without_duplicates(self):
        self.reject(lambda r, s: r["fixtures"]["case_ids"].pop(), "case_inventory")
        self.reject(lambda r, s: r["observations"]["cases"].append(copy.deepcopy(
            r["observations"]["cases"][0])), "case_inventory")

    def test_live_off_plan_requires_all_eight_named_commands(self):
        self.reject(lambda r, s: data(s, "interaction-plan-ref")["interactions"][0].update(
            command="rsvp"), "interaction_gates")

    def test_logical_interaction_ids_and_cases_cannot_be_duplicated(self):
        for field in ("interaction_id", "case_id"):
            self.reject(lambda r, s: data(s, "interaction-plan-ref")["interactions"][0].update(
                {field: data(s, "interaction-plan-ref")["interactions"][1][field]}),
                "interaction_replay")

    def test_each_interaction_is_exactly_bound_to_observation_phase_and_input(self):
        for changes in ({"phase": "negative"}, {"input_ref": "other-input"}, {"case_id": "extra-case"}):
            self.reject(lambda r, s: r["observations"]["cases"][0].update(changes),
                        "case_inventory|interaction_case")

    def test_interactions_never_target_nonconsenting_actor_or_other_channel_event(self):
        for changes in ({"actor_ref": "app"}, {"channel_ref": "feed-channel"},
                        {"event_ref": "actor"}, {"application_id": "101"}):
            self.reject(lambda r, s: data(s, "interaction-plan-ref")["interactions"][0].update(changes),
                        "interaction_resource")
        self.reject(lambda r, s: r["observations"]["cases"][0].update(resource_refs=["actor"]),
                    "interaction_resource")

    def test_one_ephemeral_ack_original_edit_and_no_followup_or_retry_budget(self):
        for changes in ({"ack_count": 0}, {"ack_count": 2}, {"edit_count": 2},
                        {"followup_count": 1}, {"ephemeral": False}, {"maximum_wire_attempts": 2},
                        {"edit_count": 1}):
            with self.subTest(changes=changes):
                self.reject(lambda r, s: data(s, "interaction-plan-ref")["interactions"][0].update(changes))
        self.reject(lambda r, s: data(s, "interaction-plan-ref")["interactions"][-1].update(
            edit_count=0), "interaction_budget")

    def test_only_one_positive_rsvp_is_supported(self):
        self.reject(lambda r, s: data(s, "interaction-plan-ref")["interactions"][-1].update(
            command="feed-add"), "unsupported_positive_interaction")

    def test_live_interaction_replay_is_explicitly_unsupported_not_fresh_budget(self):
        def mutate(r, s):
            data(s, "interaction-plan-ref")["interactions"][-1]["phase"] = "replay"
            r["observations"]["cases"][-1]["phase"] = "replay"
        self.reject(mutate, "unsupported_interaction_replay")

    def test_diagnostics_never_echo_literal_keys(self):
        receipt, records, now = input_fixture()
        data(records, "key-feed")["id"] = "private-fixture-sentinel"
        repin_inputs(receipt, records)
        with self.assertRaises(Refusal) as raised:
            self.check(receipt, records, now)
        self.assertNotIn("private-fixture-sentinel", str(raised.exception))


if __name__ == "__main__":
    unittest.main()
