"""SYNTHETIC recovery-plan comparisons, not grants, effects or complete evidence.

The minimal lifecycle fixture has no authority/telemetry/drain/ownership-fence
proof. The new resource-plan slots are deliberately unresolved placeholders;
this narrower test exercises only the new plan comparison, never qualification.
"""

import copy
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from announcement_authority_fixtures import principal
from announcement_lifecycle_fixtures import CLEANUP, KEY, OWNER, RESOURCE, lifecycle_fixture
from announcement_provenance_fixtures import at, envelope, evidence_index
from announcement_runtime_evidence import canonical_digest
from announcement_runtime_policy import runtime_preflight
from announcement_runtime_recovery import RECOVERY_PLAN_REF, check_recovery_plan_claims
from announcement_runtime_schema import Refusal


def repin(receipt, records):
    resource_plan = records["runtime-resource-plan"]["data"]
    resource_plan["recovery_plan_sha256"] = canonical_digest(records[RECOVERY_PLAN_REF]["data"])
    receipt["resources"]["plan"]["sha256"] = canonical_digest(resource_plan)
    receipt["authority"]["resource_plan"] = copy.deepcopy(receipt["resources"]["plan"])


def recovery_fixture(prior="absent"):
    receipt, records, now = lifecycle_fixture(prior)
    authority, life = receipt["authority"], receipt["lifecycle"]
    resource = receipt["resources"]["entries"][0]
    plan = {
        "purpose": "owned_recovery_only", "run": copy.deepcopy(receipt["attempt"]["run"]),
        "contract": copy.deepcopy(receipt["scope"]["contract"]),
        "policy": copy.deepcopy(authority["policy"]),
        "implementation": copy.deepcopy(authority["implementation"]),
        "candidate": copy.deepcopy(receipt["scope"]["candidate"]),
        "control": copy.deepcopy(receipt["control"]["version"]),
        "execution_ref": authority["execution"]["ref"],
        "execution_nonce_ref": authority["execution"]["nonce_ref"],
        "authority_epoch": authority["epoch"], "resource_epoch": receipt["attempt"]["enforcement_epoch"],
        "owner": principal("controller"), "served_before_sha256": canonical_digest(life["served_before"]),
        "worker_before_sha256": canonical_digest(life["worker_before"]),
        "not_before": at("00:20:00"), "expires_at": at("00:30:00"),
        "operations": [{"operation_id": CLEANUP, "resource_ref": RESOURCE,
            "method": "db_delete_owned" if prior == "absent" else "db_cas",
            "identity_sha256": canonical_digest(resource["identity"]), "key_sha256": KEY,
            "owner_ref": OWNER, "restore_state_ref": "state-before",
            "expected_count": 1, "maximum_count": 1}],
    }
    records[RECOVERY_PLAN_REF] = envelope(RECOVERY_PLAN_REF, "recovery_plan_content", plan)
    records[OWNER] = envelope(OWNER, "resource_owner", {
        "principal": principal("controller"), "execution_ref": plan["execution_ref"],
        "nonce_ref": plan["execution_nonce_ref"]})
    # Literal closed inventory, NOT generated from the production schema. Other
    # slots have no corresponding resources/evidence and must fail full checking.
    records["runtime-resource-plan"] = envelope("runtime-resource-plan", "resource_plan_content", {
        "slots": {"application_ref": "app", "actor_ref": "actor", "control_channel_ref": "control",
            "feed_channel_ref": "feed-channel", "event_ref": "event", "feed_ref": "feed",
            "delivery_ref": "delivery", "setting_ref": RESOURCE, "rsvp_ref": "rsvp",
            "rsvp_audit_ref": "audit", "source_ref": "source"},
        "entries": copy.deepcopy(receipt["resources"]["entries"]),
        "states": copy.deepcopy(receipt["resources"]["states"]),
        "identity_map": copy.deepcopy(receipt["resources"]["identity_map"]),
        "inventory_manifest": copy.deepcopy(receipt["resources"]["inventory_manifest"]),
        "operation_budgets": [{"ref": "cleanup-budget", "resource_ref": RESOURCE,
            "method": plan["operations"][0]["method"], "expected_count": 1, "maximum_count": 1}],
        "input_plan_sha256": "a" * 64, "serialization_plan_sha256": "b" * 64,
        "recovery_plan_sha256": "c" * 64,
        "not_before": at("00:00:00"), "expires_at": at("00:30:00")})
    repin(receipt, records)
    return receipt, records, now


class RecoveryPlans(unittest.TestCase):
    def reject(self, mutate, code, *, rehash=True, prior="absent"):
        receipt, records, now = recovery_fixture(prior)
        mutate(receipt, records)
        if rehash:
            repin(receipt, records)
        with self.assertRaisesRegex(Refusal, "announcement evidence refused: " + code + "$"):
            check_recovery_plan_claims(receipt, evidence_index(records), now)

    def test_absent_and_present_prior_plans_are_narrow_consistency_only(self):
        for prior in ("absent", "present"):
            with self.subTest(prior=prior):
                receipt, records, now = recovery_fixture(prior)
                self.assertLessEqual(len(records), 256)
                self.assertIsNone(check_recovery_plan_claims(receipt, evidence_index(records), now))
                self.assertFalse(receipt["promotable"])
                with self.assertRaisesRegex(Refusal, "unsupported$"):
                    runtime_preflight()

    def test_recovery_content_is_pinned_even_when_shape_is_valid(self):
        self.reject(lambda r, e: e[RECOVERY_PLAN_REF]["data"].update(expires_at=at("00:31:00")),
                    "recovery_content", rehash=False)

    def test_resource_plan_content_is_bound_to_its_receipt_digest(self):
        self.reject(lambda r, e: e["runtime-resource-plan"]["data"].update(input_plan_sha256="f" * 64),
                    "recovery_content", rehash=False)

    def test_recovery_plan_rejects_unknown_fields(self):
        self.reject(lambda r, e: e[RECOVERY_PLAN_REF]["data"].update(enabled=True), "object_fields")

    def test_copied_execution_grant_is_not_a_recovery_plan(self):
        self.reject(lambda r, e: e[RECOVERY_PLAN_REF].update(data=copy.deepcopy(r["authority"]["execution"])),
                    "object_fields")

    def test_plan_and_execution_cannot_substitute_for_recovery_authority(self):
        for ref in (RECOVERY_PLAN_REF, "execution-ref"):
            with self.subTest(ref=ref):
                receipt, records, now = recovery_fixture()
                ref = receipt["authority"]["execution"]["ref"] if ref == "execution-ref" else ref
                receipt["lifecycle"]["restoration_authority_ref"] = ref
                with self.assertRaisesRegex(Refusal, "recovery_authority_separation$"):
                    check_recovery_plan_claims(receipt, evidence_index(records), now)

    def test_scope_cannot_change_behind_rehashed_plan(self):
        replacements = {"execution_ref": "other-execution", "execution_nonce_ref": "other-nonce",
            "authority_epoch": "999", "resource_epoch": "999", "run": {"run_id": "9002",
                "attempt": 1, "control_sha": "f" * 40},
            "candidate": {"commit": "f" * 40, "tree": "e" * 40},
            "control": {"commit": "f" * 40, "tree": "e" * 40}}
        for field, wrong in replacements.items():
            with self.subTest(field=field):
                self.reject(lambda r, e: e[RECOVERY_PLAN_REF]["data"].update({field: wrong}),
                            "recovery_scope")

    def test_document_and_implementation_scope_cannot_drift(self):
        for field in ("contract", "policy", "implementation"):
            with self.subTest(field=field):
                self.reject(lambda r, e: e[RECOVERY_PLAN_REF]["data"][field].update(sha256="f" * 64),
                            "recovery_scope")

    def test_served_before_and_worker_snapshot_are_exact(self):
        for field in ("served_before_sha256", "worker_before_sha256"):
            with self.subTest(field=field):
                self.reject(lambda r, e: e[RECOVERY_PLAN_REF]["data"].update({field: "f" * 64}),
                            "recovery_snapshot")

    def test_lease_is_positive_and_at_most_sixty_minutes(self):
        for start, end in (("00:20:00", "00:20:00"), ("00:20:01", "00:20:00"),
                           ("00:20:00", "01:20:01")):
            with self.subTest(start=start, end=end):
                self.reject(lambda r, e: e[RECOVERY_PLAN_REF]["data"].update(
                    not_before=at(start), expires_at=at(end)), "recovery_lease")
        receipt, records, now = recovery_fixture()
        records[RECOVERY_PLAN_REF]["data"]["expires_at"] = at("01:20:00")
        repin(receipt, records)
        self.assertIsNone(check_recovery_plan_claims(receipt, evidence_index(records), now))

    def test_cleanup_outside_separate_lease_refuses(self):
        self.reject(lambda r, e: e[RECOVERY_PLAN_REF]["data"].update(not_before=at("00:20:31")),
                    "recovery_operation_time")

    def test_restored_sample_at_expiry_is_not_inside_lease(self):
        self.reject(lambda r, e: e[RECOVERY_PLAN_REF]["data"].update(expires_at=at("00:21:00")),
                    "recovery_window")

    def test_final_state_at_expiry_is_not_verified_restoration(self):
        self.reject(lambda r, e: e[RECOVERY_PLAN_REF]["data"].update(expires_at=at("00:22:00")),
                    "recovery_final_time")

    def test_owner_cannot_alias_author_executor_caller_or_reviewer_subject(self):
        for role in ("author", "executor", "caller", "security", "qa"):
            def mutate(receipt, records):
                a = receipt["authority"]
                who = a[role] if role in ("author", "executor", "caller") else next(
                    d["principal"] for d in a["decisions"] if d["role"] == role)
                records[RECOVERY_PLAN_REF]["data"]["owner"]["subject_id"] = who["subject_id"]
            with self.subTest(role=role):
                self.reject(mutate, "recovery_owner")

    def test_owner_mapping_execution_and_nonce_are_complete(self):
        for field, wrong in (("execution_ref", "other-execution"), ("nonce_ref", "other-nonce"),
                             ("principal", principal("verifier"))):
            with self.subTest(field=field):
                self.reject(lambda r, e: e[OWNER]["data"].update({field: wrong}), "recovery_owner")

    def test_extra_missing_and_renamed_cleanup_operations_refuse(self):
        self.reject(lambda r, e: e[RECOVERY_PLAN_REF]["data"]["operations"][0].update(
            operation_id="different-cleanup"), "recovery_operations")
        self.reject(lambda r, e: e[RECOVERY_PLAN_REF]["data"].update(operations=[]), "array_size")
        self.reject(lambda r, e: e[RECOVERY_PLAN_REF]["data"]["operations"].append(copy.deepcopy(
            e[RECOVERY_PLAN_REF]["data"]["operations"][0])), "reference_duplicate")

    def test_resource_key_owner_and_prior_target_cannot_be_broadened(self):
        for field, wrong in (("identity_sha256", "f" * 64), ("key_sha256", "f" * 64),
                             ("owner_ref", "other-owner"), ("restore_state_ref", "state-setup")):
            with self.subTest(field=field):
                self.reject(lambda r, e: e[RECOVERY_PLAN_REF]["data"]["operations"][0].update(
                    {field: wrong}), "recovery_resource")

    def test_business_reads_inserts_and_sends_are_not_recovery_methods(self):
        for method in ("db_read", "db_insert", "feed_post", "message_delete_owned"):
            with self.subTest(method=method):
                self.reject(lambda r, e: e[RECOVERY_PLAN_REF]["data"]["operations"][0].update(
                    method=method), "enum")

    def test_recovery_counts_are_exact_one_not_slack_or_bool(self):
        for field in ("expected_count", "maximum_count"):
            for wrong in (0, 2, True, 1.0):
                with self.subTest(field=field, wrong=wrong):
                    self.reject(lambda r, e: e[RECOVERY_PLAN_REF]["data"]["operations"][0].update(
                        {field: wrong}), "scalar_type" if type(wrong) is float else "integer")

    def test_coherent_cleanup_method_must_restore_prior_presence(self):
        for prior, method in (("absent", "db_cas"), ("present", "db_delete_owned")):
            def mutate(receipt, records):
                records[RECOVERY_PLAN_REF]["data"]["operations"][0]["method"] = method
                effect = receipt["observations"]["actual_effects"][-1]
                effect["method"] = method
                records[effect["db_transition_refs"][0]]["data"]["method"] = method
            with self.subTest(prior=prior):
                self.reject(mutate, "recovery_method", prior=prior)

    def test_empty_plan_cannot_be_accepted_by_type_alias(self):
        self.reject(lambda r, e: e[RECOVERY_PLAN_REF].update(kind="execution_authority"),
                    "object_fields")


if __name__ == "__main__":
    unittest.main()
