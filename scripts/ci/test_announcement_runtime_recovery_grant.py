"""SYNTHETIC separate recovery grants, not authenticated authority or cleanup.

The merged partial fixtures deliberately do not satisfy runtime scope, current
recovery reads/history, boundary fences or full-packet accounting. Only the new
plan/grant/decision/nonce comparisons are exercised; cleanup must still refuse.
"""

import copy
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from announcement_authority_fixtures import authority_fixture, decision, principal
from announcement_provenance_fixtures import at, envelope, evidence_index
from announcement_runtime_evidence import RECOVERY_RECORDS, canonical_digest
from announcement_runtime_lifecycle import check_lifecycle_phase_claims
from announcement_runtime_policy import runtime_preflight
from announcement_runtime_recovery import RECOVERY_PLAN_REF, check_recovery_grant_claims
from announcement_runtime_schema import Refusal
from test_announcement_runtime_recovery import recovery_fixture, repin

GRANT, SCOPE, NONCE, CONSUMPTION = (
    "separate-recovery", "separate-recovery-scope", "separate-recovery-nonce", "separate-recovery-use")


def grant_fixture(prior="absent"):
    receipt, records, now = recovery_fixture(prior)
    runtime, authority_records, _ = authority_fixture()
    records = {**authority_records, **records}
    receipt["authority"] = copy.deepcopy(runtime["authority"])
    receipt["control"] = copy.deepcopy(runtime["control"])
    receipt["attempt"]["actor"] = copy.deepcopy(runtime["attempt"]["actor"])
    receipt["attempt"]["enforcement_epoch"] = runtime["attempt"]["enforcement_epoch"]
    authority, attempt = receipt["authority"], receipt["attempt"]
    plan = records[RECOVERY_PLAN_REF]["data"]
    plan.update(policy=copy.deepcopy(authority["policy"]),
        implementation=copy.deepcopy(authority["implementation"]),
        control=copy.deepcopy(receipt["control"]["version"]),
        execution_ref=authority["execution"]["ref"], execution_nonce_ref=authority["execution"]["nonce_ref"],
        authority_epoch=authority["epoch"], resource_epoch=attempt["enforcement_epoch"])
    records["owner-ref"]["data"].update(execution_ref=plan["execution_ref"],
                                         nonce_ref=plan["execution_nonce_ref"])
    repin(receipt, records)
    source = records[authority["source_ref"]]["data"]
    consumers = {role: records[source[role + "_ref"]]["data"] for role in ("controller", "verifier")}
    scope = {
        "purpose": "owned_recovery_only", "runtime_scope_sha256": authority["scope_sha256"],
        "source_sha256": canonical_digest(source), "consumers_sha256": canonical_digest(consumers),
        "plan_ref": RECOVERY_PLAN_REF, "plan_sha256": canonical_digest(plan),
        "resource_plan": copy.deepcopy(receipt["resources"]["plan"]), "owner": copy.deepcopy(plan["owner"]),
        "run": copy.deepcopy(attempt["run"]), "epoch": "11", "resource_epoch": attempt["enforcement_epoch"],
    }
    records[SCOPE] = envelope(SCOPE, "recovery_scope", scope)
    grant = {"ref": GRANT, "purpose": "owned_recovery_only", "source_ref": authority["source_ref"],
        "scope_ref": SCOPE, "scope_sha256": canonical_digest(scope), "epoch": "11",
        "resource_epoch": attempt["enforcement_epoch"], "generation": "12",
        "run": copy.deepcopy(attempt["run"]), "owner": copy.deepcopy(plan["owner"]),
        "nonce_ref": NONCE, "consumption_ref": CONSUMPTION,
        "not_before": plan["not_before"], "expires_at": plan["expires_at"],
        "decisions": [decision(role, canonical_digest(scope), recovery=True) for role in ("security", "qa")]}
    records[GRANT] = envelope(GRANT, "recovery_authority", grant)
    for d in grant["decisions"]:
        records[d["ref"]] = envelope(d["ref"], "authority_decision", {
            "claim": copy.deepcopy(d), "source_ref": authority["source_ref"],
            "security_round": d["round"], "generation": "12"})
    records[CONSUMPTION] = envelope(CONSUMPTION, "recovery_nonce_consumption", {
        "source_ref": authority["source_ref"], "recovery_ref": GRANT, "scope_sha256": canonical_digest(scope),
        "nonce_ref": NONCE, "run": copy.deepcopy(attempt["run"]), "owner_ref": "principal-controller",
        "consumed_at": at("00:20:00"), "authority_epoch": "11",
        "resource_epoch": attempt["enforcement_epoch"], "outcome": "consumed"})
    receipt["lifecycle"]["restoration_authority_ref"] = GRANT
    return receipt, records, now


def repin_grant(records):
    grant = records[GRANT]["data"]
    grant["scope_sha256"] = canonical_digest(records[SCOPE]["data"])
    for d in grant["decisions"]:
        d["scope_sha256"] = grant["scope_sha256"]
        records[d["ref"]]["data"]["claim"] = copy.deepcopy(d)
    records[CONSUMPTION]["data"]["scope_sha256"] = grant["scope_sha256"]


class RecoveryGrants(unittest.TestCase):
    def reject(self, mutate, code, *, rehash=False):
        receipt, records, now = grant_fixture()
        mutate(receipt, records)
        if rehash:
            repin_grant(records)
        with self.assertRaisesRegex(Refusal, "announcement evidence refused: " + code + "$"):
            check_recovery_grant_claims(receipt, evidence_index(records), now)

    def test_coherent_separate_grants_are_only_partial_consistency(self):
        for prior in ("absent", "present"):
            with self.subTest(prior=prior):
                receipt, records, now = grant_fixture(prior)
                self.assertLessEqual(len(records), 256)
                self.assertIsNone(check_recovery_grant_claims(receipt, evidence_index(records), now))
                self.assertFalse(receipt["promotable"])
                with self.assertRaisesRegex(Refusal, "unsupported$"):
                    runtime_preflight()

    def test_plan_only_fixture_cannot_satisfy_grant_check(self):
        receipt, records, now = recovery_fixture()
        with self.assertRaisesRegex(Refusal, "unresolved_reference$"):
            check_recovery_grant_claims(receipt, evidence_index(records), now)

    def test_each_new_record_is_closed_and_nonempty(self):
        for ref in (GRANT, SCOPE, CONSUMPTION):
            for wrong in ({}, None, "extra"):
                def mutate(r, e):
                    e[ref]["data"] = (dict(e[ref]["data"], extra=True) if wrong == "extra" else wrong)
                with self.subTest(ref=ref, wrong=wrong):
                    self.reject(mutate, "object_fields")
        self.assertTrue({"recovery_scope", "recovery_authority", "recovery_nonce_consumption"}.issubset(
            RECOVERY_RECORDS))

    def test_copied_runtime_grant_and_nonce_are_wrong_kinds(self):
        for ref, old in ((GRANT, "execution-ref"), (CONSUMPTION, "consumption-ref")):
            def mutate(r, e):
                e[ref] = copy.deepcopy(e[old])
                e[ref]["ref"] = ref
            with self.subTest(ref=ref):
                self.reject(mutate, "reference_kind")

    def test_missing_or_mixed_attempt_records_refuse(self):
        for ref in (GRANT, SCOPE, CONSUMPTION, "recovery-security", "recovery-qa"):
            with self.subTest(ref=ref):
                self.reject(lambda r, e: e.pop(ref), "unresolved_reference")
                self.reject(lambda r, e: e[ref]["run"].update(attempt=2), "evidence_attempt")

    def test_source_cannot_accept_caller_endpoint_key_or_adapter(self):
        for field in ("endpoint", "public_key", "adapter", "allow"):
            with self.subTest(field=field):
                self.reject(lambda r, e: e["authority-source"]["data"].update({field: "sentinel"}),
                            "object_fields")

    def test_grant_identity_run_source_owner_and_resource_epoch_are_exact(self):
        wrong = {"ref": "other-grant", "source_ref": "other-source", "resource_epoch": "99",
                 "owner": principal("verifier"),
                 "run": {"run_id": "999", "attempt": 1, "control_sha": "f" * 40}}
        for field, value in wrong.items():
            with self.subTest(field=field):
                self.reject(lambda r, e: e[GRANT]["data"].update({field: value}), "recovery_grant")

    def test_scope_digest_is_not_just_a_label(self):
        self.reject(lambda r, e: e[GRANT]["data"].update(scope_sha256="f" * 64),
                    "recovery_grant_scope")

    def test_rehashed_scope_cannot_change_bound_content(self):
        for field in ("runtime_scope_sha256", "source_sha256", "consumers_sha256", "plan_sha256"):
            with self.subTest(field=field):
                self.reject(lambda r, e: e[SCOPE]["data"].update({field: "f" * 64}),
                            "recovery_grant_scope", rehash=True)
        for field, wrong in (("plan_ref", "other-plan"), ("epoch", "99"), ("resource_epoch", "99"),
                             ("owner", principal("verifier"))):
            with self.subTest(field=field):
                self.reject(lambda r, e: e[SCOPE]["data"].update({field: wrong}),
                            "recovery_grant_scope", rehash=True)

    def test_rehashed_resource_plan_and_run_substitution_refuse(self):
        self.reject(lambda r, e: e[SCOPE]["data"]["resource_plan"].update(sha256="f" * 64),
                    "recovery_grant_scope", rehash=True)
        self.reject(lambda r, e: e[SCOPE]["data"]["run"].update(attempt=2),
                    "recovery_grant_scope", rehash=True)

    def test_grant_cannot_shorten_or_extend_approved_plan_lease(self):
        for field, clock in (("not_before", "00:19:59"), ("not_before", "00:20:01"),
                             ("expires_at", "00:29:59"), ("expires_at", "00:30:01")):
            with self.subTest(field=field, clock=clock):
                self.reject(lambda r, e: e[GRANT]["data"].update({field: at(clock)}),
                            "recovery_grant_lease")

    def test_recovery_nonce_cannot_reuse_any_runtime_reference(self):
        for field in ("nonce_ref", "consumption_ref"):
            for runtime_field in ("ref", "nonce_ref", "consumption_ref"):
                with self.subTest(field=field, runtime_field=runtime_field):
                    self.reject(lambda r, e: e[GRANT]["data"].update(
                        {field: r["authority"]["execution"][runtime_field]}), "recovery_grant_separation")
        self.reject(lambda r, e: e[GRANT]["data"].update(nonce_ref=CONSUMPTION),
                    "recovery_grant_separation")

    def test_recovery_needs_its_own_complete_stage_mapping(self):
        self.reject(lambda r, e: e["authority-source"]["data"]["stage_bindings"].pop(), "array_size")
        self.reject(lambda r, e: e["authority-source"]["data"]["stage_bindings"][-1].update(
            purpose="runtime"), "stage_mapping")

    def test_recovery_decisions_are_ordered_same_scope_and_same_epoch(self):
        self.reject(lambda r, e: e[GRANT]["data"]["decisions"].reverse(), "decision_order")
        self.reject(lambda r, e: e[GRANT]["data"]["decisions"][1].update(scope_sha256="f" * 64),
                    "decision_scope")
        self.reject(lambda r, e: e[GRANT]["data"]["decisions"][1].update(epoch="7"), "decision_scope")
        self.reject(lambda r, e: e[GRANT]["data"]["decisions"][1].update(decided_at=at("00:20:01")),
                    "decision_order")

    def test_recovery_cannot_borrow_runtime_or_installation_decisions(self):
        for ref in ("runtime-security", "install-security"):
            def mutate(r, e):
                d = copy.deepcopy(r["authority"]["decisions"][0] if ref == "runtime-security" else
                    e["installation-approval-0"]["data"]["decisions"][0])
                d.update(scope_sha256=e[GRANT]["data"]["scope_sha256"], epoch="11")
                e[GRANT]["data"]["decisions"][0] = d
            with self.subTest(ref=ref):
                self.reject(mutate, "security_round_binding")

    def test_recovery_reviewers_cannot_alias_other_principals(self):
        for role in ("author", "executor", "caller", "controller", "verifier", "qa"):
            with self.subTest(role=role):
                self.reject(lambda r, e: e[GRANT]["data"]["decisions"][0].update(
                    principal=principal(role)), "decision_scope")

    def test_decision_identity_cannot_alias_prior_decisions_with_new_scope(self):
        for field in ("ref", "decision_id"):
            def mutate(r, e):
                old = r["authority"]["decisions"][1][field]
                e[GRANT]["data"]["decisions"][1][field] = old
            with self.subTest(field=field):
                self.reject(mutate, "recovery_decision_separation")

    def test_decision_stage_participant_and_security_round_are_bound(self):
        for field in ("stage_id", "participant_id"):
            with self.subTest(field=field):
                self.reject(lambda r, e: e[GRANT]["data"]["decisions"][0].update(
                    {field: "00000000-0000-4000-8000-000000000000"}), "decision_stage")
        self.reject(lambda r, e: e[GRANT]["data"]["decisions"][1].update(
            security_decision_id="00000000-0000-4000-8000-000000000000"), "security_round_binding")

    def test_decision_sidecars_bind_generation_source_round_and_complete_claim(self):
        for field, wrong in (("generation", "8"), ("source_ref", "other-source"), ("security_round", 1)):
            with self.subTest(field=field):
                self.reject(lambda r, e: e["recovery-qa"]["data"].update({field: wrong}),
                            "recovery_decision_binding")
        self.reject(lambda r, e: e["recovery-qa"]["data"]["claim"].update(round=3),
                    "recovery_decision_binding")

    def test_replayed_unknown_and_wrong_scope_nonce_use_refuse(self):
        for field, wrong in (("outcome", "replayed"), ("outcome", "unknown"),
                ("source_ref", "other-source"), ("recovery_ref", "execution-ref"),
                ("scope_sha256", "f" * 64), ("nonce_ref", "one-shot-nonce"),
                ("owner_ref", "principal-verifier"), ("authority_epoch", "7"), ("resource_epoch", "99")):
            with self.subTest(field=field, wrong=wrong):
                self.reject(lambda r, e: e[CONSUMPTION]["data"].update({field: wrong}),
                            "recovery_nonce_binding")
        self.reject(lambda r, e: e[CONSUMPTION]["data"]["run"].update(attempt=2),
                    "recovery_nonce_binding")

    def test_nonce_consumption_precedes_first_cleanup_and_fits_lease(self):
        for clock in ("00:19:59", "00:20:31", "00:30:00"):
            with self.subTest(clock=clock):
                self.reject(lambda r, e: e[CONSUMPTION]["data"].update(consumed_at=at(clock)),
                            "recovery_nonce_time")
        receipt, records, now = grant_fixture()
        records[CONSUMPTION]["data"]["consumed_at"] = at("00:20:30")
        self.assertIsNone(check_recovery_grant_claims(receipt, evidence_index(records), now))

    def test_missing_current_reads_and_fences_still_prevent_lifecycle_acceptance(self):
        receipt, records, now = grant_fixture()
        self.assertIsNone(check_recovery_grant_claims(receipt, evidence_index(records), now))
        # Existing minimal runtime checkpoint claims are intentionally incomplete;
        # the phase checker must refuse, never treat the separate grant as a fence.
        with self.assertRaises(Refusal):
            check_lifecycle_phase_claims(receipt, evidence_index(records), now)


if __name__ == "__main__":
    unittest.main()
