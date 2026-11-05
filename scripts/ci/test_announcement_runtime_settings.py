"""Hermetic setting/value/reader adversaries; no stored data or runtime proof."""

import copy
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from announcement_provenance_fixtures import at, evidence_index
from announcement_resource_fixtures import repin_authority, repin_resources
from announcement_scheduler_fixtures import repin_scheduler
from announcement_setting_fixtures import SETTING_OPS, repin_settings, setting_fixture
from announcement_runtime_evidence import canonical_digest
from announcement_runtime_policy import runtime_preflight
from announcement_runtime_schema import Refusal
from announcement_runtime_settings import check_setting_claims
from announcement_runtime_transitions import check_transition_claims


def plan(records):
    return records["runtime-setting-plan"]["data"]


def value(records, number):
    return records["setting-value-" + str(number)]["data"]


def state(records, number):
    return records[value(records, number)["state_ref"]]["data"]["claim"]


def prior_string(receipt, records, prior):
    """Coherently model a preexisting explicit string-bit baseline and restore.

    Fabricated positive versions are legacy row-version claims, NOT C's signed
    database-owned CAS tokens. No trigger/audit/provenance enforcement is proved.
    """
    entries = {entry["ref"]: entry for entry in receipt["resources"]["entries"]}
    resource = entries["setting"]
    resource["allowed_operations"] = ["db_cas"]
    budgets = records["runtime-resource-plan"]["data"]["operation_budgets"]
    budgets[:] = [budget for budget in budgets if budget["resource_ref"] != "setting"]
    budgets.append({"ref": "budget-setting-all-cas", "resource_ref": "setting",
                    "method": "db_cas", "expected_count": 4, "maximum_count": 4})
    for number, stored, version in ((0, prior, "10"), (1, "0", "11"), (2, "1", "12"),
                                    (3, "0", "13"), (4, prior, "14"), (5, prior, "14")):
        value(records, number)["value"] = {"presence": "present", "stored_value": stored}
        claim = state(records, number)
        claim.update(presence="present", version=version, value_sha256=canonical_digest(stored))
        if number in (0, 5):
            entry = next(x for x in receipt["resources"]["states"] if x["ref"] == claim["ref"])
            entry.clear()
            entry.update(copy.deepcopy(claim))
    for effect in receipt["observations"]["actual_effects"]:
        if effect["operation_id"] not in SETTING_OPS:
            continue
        effect["method"] = "db_cas"
        transition = records[effect["db_transition_refs"][0]]["data"]
        transition["method"] = "db_cas"
        records[transition["claim"]["ownership_match_ref"]]["data"]["method"] = "db_cas"
    receipt["observations"]["expected_effects"] = copy.deepcopy(receipt["observations"]["actual_effects"])
    for boundary, version in zip(receipt["fixtures"]["setting_boundaries"], ("11", "12", "12", "13")):
        boundary["row_version"] = version
        records[boundary["reader_receipt_ref"]]["data"]["row_version"] = version
    repin_resources(receipt, records)
    repin_settings(receipt, records)


class Settings(unittest.TestCase):
    def reject(self, mutate, code, *, repin=True):
        receipt, records, now = setting_fixture()
        mutate(receipt, records)
        if repin:
            repin_settings(receipt, records)
        with self.assertRaisesRegex(Refusal, "announcement evidence refused: " + code + "$"):
            check_setting_claims(receipt, evidence_index(records), now)

    def test_fabricated_setting_graph_is_closed_and_grants_no_runtime_capability(self):
        receipt, records, now = setting_fixture()
        self.assertLessEqual(len(records), 256)
        index = evidence_index(records)
        self.assertIsNone(check_setting_claims(receipt, index, now))
        index.require_all_used()
        self.assertFalse(receipt["promotable"])
        with self.assertRaisesRegex(Refusal, "unsupported"):
            runtime_preflight()

    def test_compaction_keeps_setting_and_business_reads_at_their_operation_time(self):
        receipt, records, now = setting_fixture()
        self.assertLessEqual(len(records), 256)
        checkpoints = {x["ref"]: x for x in receipt["attempt"]["checkpoints"]}
        self.assertEqual(at("00:03:00"), checkpoints["checkpoint-0"]["checked_at"])
        self.assertEqual(at("00:08:00"), checkpoints["checkpoint-3"]["checked_at"])
        self.assertEqual(at("00:08:00"), checkpoints["checkpoint-4"]["checked_at"])
        phases = []
        for operation in SETTING_OPS:
            transition = records["transition-" + operation]["data"]["claim"]
            checkpoint = checkpoints[transition["authority_ref"]]
            phases.append(checkpoint["phase"])
            self.assertEqual(transition["observed_at"], checkpoint["checked_at"])
            for role in ("controller", "verifier"):
                read = records[checkpoint[role + "_read_ref"]]["data"]["claim"]
                self.assertEqual(transition["observed_at"], read["read_at"])
        self.assertEqual(["effect_batch", "transition", "transition", "effect_batch"], phases)
        index = evidence_index(records)
        self.assertIsNone(check_setting_claims(receipt, index, now))
        index.require_all_used()

    def test_compacted_setting_enable_cannot_borrow_same_time_business_authority(self):
        receipt, records, now = setting_fixture()
        self.assertIsNone(check_setting_claims(receipt, evidence_index(records), now))
        transition = records["transition-" + SETTING_OPS[1]]["data"]["claim"]
        transition["authority_ref"] = "checkpoint-4"
        records[transition["ownership_match_ref"]]["data"]["checkpoint_ref"] = "checkpoint-4"
        with self.assertRaisesRegex(Refusal, "transition_authority$"):
            check_setting_claims(receipt, evidence_index(records), now)

    def test_present_zero_and_one_baselines_restore_value_without_resetting_version(self):
        for prior in ("0", "1"):
            with self.subTest(prior=prior):
                receipt, records, now = setting_fixture()
                prior_string(receipt, records, prior)
                index = evidence_index(records)
                self.assertIsNone(check_setting_claims(receipt, index, now))
                index.require_all_used()
                self.assertEqual(prior, value(records, 5)["value"]["stored_value"])
                self.assertNotEqual(state(records, 0)["version"], state(records, 5)["version"])

    def test_new_fixture_does_not_claim_complete_lifecycle_or_actual_cas_tokens(self):
        receipt, records, _ = setting_fixture()
        effects = receipt["observations"]["actual_effects"]
        settings = [x for x in effects if x["operation_id"] in SETTING_OPS]
        self.assertEqual([at(t) for t in ("00:03:00", "00:08:00", "00:12:00", "00:18:00")],
                         [x["observed_at"] for x in settings])
        self.assertEqual({at("00:08:00")}, {x["observed_at"] for x in effects if x not in settings})
        self.assertEqual(["1", "2", "3"], [state(records, n)["version"] for n in (1, 2, 3)])
        self.assertEqual(receipt["rollout"]["started_at"], settings[0]["observed_at"])

    def test_coherent_wrong_state_values_pass_generic_chain_but_not_setting_semantics(self):
        receipt, records, now = setting_fixture()
        value(records, 1)["value"]["stored_value"] = "1"
        state(records, 1)["value_sha256"] = canonical_digest("1")
        repin_settings(receipt, records)
        self.assertIsNone(check_transition_claims(receipt, evidence_index(records), now))
        with self.assertRaisesRegex(Refusal, "setting_value$"):
            check_setting_claims(receipt, evidence_index(records), now)

    def test_each_baseline_on_and_final_off_value_is_exact(self):
        for number, wrong in ((1, "1"), (2, "0"), (3, "1")):
            def mutate(receipt, records):
                value(records, number)["value"]["stored_value"] = wrong
                state(records, number)["value_sha256"] = canonical_digest(wrong)
            with self.subTest(state=number):
                self.reject(mutate, "setting_value")

    def test_value_hash_must_be_the_json_string_not_numeric_zero_or_one(self):
        for number in (1, 2, 3):
            with self.subTest(state=number):
                self.reject(lambda r, e: state(e, number).update(
                    value_sha256=canonical_digest(int(value(e, number)["value"]["stored_value"]))),
                    "setting_value_digest")

    def test_restoration_to_another_valid_string_bit_is_not_baseline_restoration(self):
        def mutate(receipt, records):
            prior_string(receipt, records, "1")
            for number in (4, 5):
                value(records, number)["value"]["stored_value"] = "0"
                state(records, number)["value_sha256"] = canonical_digest("0")
            final = next(x for x in receipt["resources"]["states"] if x["ref"] == "after-setting")
            final["value_sha256"] = canonical_digest("0")
            repin_resources(receipt, records)
        self.reject(mutate, "setting_restore_value")

    def test_projection_presence_cannot_disagree_with_observed_state(self):
        self.reject(lambda r, e: value(e, 1).update(value={"presence": "absent"}),
                    "setting_value_presence")

    def test_plan_and_each_value_need_content_hashes(self):
        self.reject(lambda r, e: plan(e).update(setting_ref="feed"), "setting_content", repin=False)
        self.reject(lambda r, e: value(e, 1).update(key_ref="key-feed"),
                    "setting_content", repin=False)

    def test_setting_resource_cannot_be_substituted_after_repin(self):
        self.reject(lambda r, e: plan(e).update(setting_ref="feed"), "setting_resource")

    def test_plan_key_cannot_be_another_literal_key(self):
        self.reject(lambda r, e: plan(e).update(key=copy.deepcopy(
            next(x["key"] for x in e["runtime-input-plan"]["data"]["keys"]
                 if x["resource_ref"] == "feed"))), "setting_key")

    def test_value_projection_must_name_the_literal_setting_key(self):
        self.reject(lambda r, e: value(e, 2).update(key_ref="key-feed"), "setting_value_inventory")

    def test_value_inventory_rejects_an_unrelated_state_or_duplicate_state(self):
        for ref in ("before-feed", "before-setting"):
            with self.subTest(ref=ref):
                self.reject(lambda r, e: value(e, 2).update(state_ref=ref), "setting_value_inventory")

    def test_value_pins_cannot_reuse_a_projection(self):
        self.reject(lambda r, e: plan(e)["values"].__setitem__(2,
                    copy.deepcopy(plan(e)["values"][1])), "reference_duplicate")

    def test_each_operation_is_ordered_and_bound_to_exact_setting_writes(self):
        for phase in ("baseline", "enable", "disable", "restore"):
            with self.subTest(phase=phase):
                self.reject(lambda r, e: plan(e)["operations"].update({phase: "unrelated-operation"}),
                            "setting_operations")

    def test_swapped_on_and_off_operations_are_refused(self):
        def mutate(receipt, records):
            ops = plan(records)["operations"]
            ops["enable"], ops["disable"] = ops["disable"], ops["enable"]
        self.reject(mutate, "setting_operations")

    def test_reader_version_must_be_committed_state_not_plausible_increasing_numbers(self):
        def mutate(receipt, records):
            for boundary in receipt["fixtures"]["setting_boundaries"]:
                boundary["row_version"] = str(int(boundary["row_version"]) + 10)
                records[boundary["reader_receipt_ref"]]["data"]["row_version"] = boundary["row_version"]
        self.reject(mutate, "setting_reader_state")

    def test_boundary_cannot_read_on_before_enable_commits(self):
        def mutate(receipt, records):
            boundary = receipt["fixtures"]["setting_boundaries"][1]
            boundary["read_at"] = at("00:07:59")
            schedule = records[receipt["fixtures"]["scheduler_plan_ref"]]["data"]
            schedule["windows"][0]["ended_at"] = at("00:07:59")
            schedule["windows"][1]["started_at"] = at("00:07:59")
            records[boundary["reader_receipt_ref"]]["data"]["read_at"] = at("00:07:59")
            for case in receipt["observations"]["cases"]:
                if case["phase"] == "off":
                    case["ended_at"] = at("00:07:59")
                elif case["phase"] == "on":
                    case["started_at"] = at("00:07:59")
        self.reject(mutate, "setting_reader_state")

    def test_off_poll_cannot_borrow_snapshot_after_enable(self):
        def mutate(receipt, records):
            schedule = records[receipt["fixtures"]["scheduler_plan_ref"]]["data"]
            records[schedule["windows"][0]["poll"]["ref"]]["data"]["observed_at"] = at("00:08:00")
        self.reject(mutate, "setting_poll_state")

    def test_replay_poll_cannot_survive_final_off_mutation(self):
        def mutate(receipt, records):
            schedule = records[receipt["fixtures"]["scheduler_plan_ref"]]["data"]
            records[schedule["windows"][2]["poll"]["ref"]]["data"]["observed_at"] = at("00:12:00")
        self.reject(mutate, "setting_poll_state")

    def test_missing_or_unknown_projection_kind_is_refused(self):
        self.reject(lambda r, e: e.pop("setting-value-1"), "unresolved_reference", repin=False)
        self.reject(lambda r, e: e["setting-value-1"].update(kind="document"),
                    "object_fields", repin=False)

    def test_projection_and_plan_are_closed_and_required(self):
        for ref in ("setting-value-1", "runtime-setting-plan"):
            with self.subTest(ref=ref):
                self.reject(lambda r, e: e[ref]["data"].update(extra=True), "object_fields", repin=False)
                self.reject(lambda r, e: e[ref]["data"].pop(next(iter(e[ref]["data"]))),
                            "object_fields", repin=False)

    def test_null_numeric_boolean_and_nonbit_string_values_are_not_explicit_stored_bits(self):
        for wrong in (None, 0, 1, False, True, "", "null", "true", " 0", "01", "２"):
            with self.subTest(wrong=wrong):
                self.reject(lambda r, e: value(e, 1)["value"].update(stored_value=wrong),
                            "enum", repin=False)

    def test_absent_projection_cannot_hide_a_value(self):
        self.reject(lambda r, e: value(e, 0)["value"].update(stored_value="0"),
                    "object_fields", repin=False)

    def test_wrong_attempt_projection_is_refused(self):
        self.reject(lambda r, e: e["setting-value-1"]["run"].update(attempt=2),
                    "evidence_attempt", repin=False)

    def test_each_projection_record_and_state_reference_is_required(self):
        for number in range(6):
            with self.subTest(number=number):
                self.reject(lambda r, e: value(e, number).update(state_ref="unresolved-state"),
                            "setting_value_inventory")

    def test_duplicate_plan_operations_do_not_create_an_extra_setting_budget(self):
        self.reject(lambda r, e: plan(e)["operations"].update(restore=SETTING_OPS[2]),
                    "setting_operations")


if __name__ == "__main__":
    unittest.main()
