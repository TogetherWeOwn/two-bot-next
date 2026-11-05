"""Hermetic adverse authority graphs, never actual approval/runtime evidence."""

import copy
from datetime import timedelta
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from announcement_authority_fixtures import authority_fixture
from announcement_provenance_fixtures import at, evidence_index
from announcement_runtime_authority import check_authority_graph_claims
from announcement_runtime_evidence import AUTHORITY_RECORDS, canonical_digest
from announcement_runtime_policy import runtime_preflight
from announcement_runtime_provenance import check_provenance_claims
from announcement_runtime_schema import Refusal


def rehash_history(receipt, records, ref):
    """Keep copied hashes coherent to test semantics beyond byte equality."""
    digest = canonical_digest(records[ref]["data"])
    for record in records.values():
        if record["kind"] == "authority_read" and record["data"]["history_ref"] == ref:
            claim = record["data"]["claim"]
            claim["history_sha256"] = digest
            for copied in receipt["authority"]["reads"]:
                if copied["ref"] == claim["ref"]:
                    copied["history_sha256"] = digest


class AuthorityGraph(unittest.TestCase):
    def check(self, receipt, records, now):
        return check_authority_graph_claims(receipt, evidence_index(records), now)

    def reject(self, mutate, code=None):
        receipt, records, now = authority_fixture()
        mutate(receipt, records)
        with self.assertRaisesRegex(Refusal, code or "announcement evidence refused"):
            self.check(receipt, records, now)

    def test_coherent_claims_return_no_authority_and_preflight_still_refuses(self):
        r, s, now = authority_fixture()
        index = evidence_index(s)
        self.assertIsNone(check_authority_graph_claims(r, index, now))
        self.assertIsNone(check_provenance_claims(r, index, now))
        index.require_all_used()
        with self.assertRaisesRegex(Refusal, "unsupported"):
            runtime_preflight()

    def test_every_authority_kind_is_closed_and_nonempty(self):
        _, records, _ = authority_fixture()
        examples = {r["kind"]: ref for ref, r in records.items() if r["kind"] in AUTHORITY_RECORDS}
        self.assertEqual(set(examples), set(AUTHORITY_RECORDS))
        for kind, ref in examples.items():
            for changed in ({}, {**records[ref]["data"], "extra": True}, None):
                with self.subTest(kind=kind, changed=type(changed)):
                    self.reject(lambda r, s: s[ref].update(data=copy.deepcopy(changed)))

    def test_missing_wrong_type_and_other_attempt_sidecars_refuse(self):
        for ref in ("authority-source", "runtime-authority-scope", "principal-security",
                    "verifier-consumer", "runtime-security", "execution-ref", "consumption-ref",
                    "runtime-read-controller-0", "runtime-history-0", "fence-0",
                    "installation-approval-0", "install-read-controller-0", "install-history-0"):
            with self.subTest(ref=ref):
                self.reject(lambda r, s: s.pop(ref), "unresolved_reference")
                self.reject(lambda r, s: s[ref]["run"].update(attempt=2), "evidence_attempt")
                self.reject(lambda r, s: s[ref].update(kind="document"))

    def test_caller_cannot_select_endpoint_key_or_adapter(self):
        for field in ("endpoint", "public_key", "adapter", "allow", "trust_root"):
            with self.subTest(field=field):
                self.reject(lambda r, s: s["authority-source"]["data"].update({field: "sentinel"}),
                            "object_fields")

    def test_source_repo_policy_and_installation_mapping_are_exact(self):
        for field, value in (("installation_mapping_ref", "other-mapping"),
                             ("policy", {"ref": "other-policy", "revision": "a" * 8 +
                                          "-0000-4000-8000-000000000000", "sha256": "a" * 64})):
            with self.subTest(field=field):
                self.reject(lambda r, s: s["authority-source"]["data"].update({field: value}),
                            "authority_source")
        self.reject(lambda r, s: s["authority-source"]["data"]["repository"].update(id="2"))

    def test_principals_cannot_alias_roles_subjects_or_ref_labels(self):
        mutations = [
            lambda r, s: s["principal-security"]["data"]["principal"].update(ref="principal-qa"),
            lambda r, s: s["principal-security"]["data"].update(roles=["security", "qa"]),
            lambda r, s: s["principal-security"]["data"].update(roles=["executor"]),
            lambda r, s: s["principal-security"]["data"]["principal"].update(
                subject_id=s["principal-executor"]["data"]["principal"]["subject_id"]),
            lambda r, s: s["principal-verifier"]["data"]["principal"].update(
                subject_id=s["principal-controller"]["data"]["principal"]["subject_id"]),
            lambda r, s: r["authority"].update(author=r["authority"]["executor"]),
        ]
        for mutate in mutations:
            with self.subTest(mutate=mutate):
                self.reject(mutate)

    def test_controller_and_verifier_need_distinct_consumer_claims(self):
        for change in ({"role": "controller"}, {"provisioning_ref": "controller-provisioning"}):
            with self.subTest(change=change):
                self.reject(lambda r, s: s["verifier-consumer"]["data"].update(change),
                            "consumer_independence")
        self.reject(lambda r, s: s["verifier-consumer"]["data"].update(
            principal=s["controller-consumer"]["data"]["principal"]), "consumer_independence")
        self.reject(lambda r, s: s["runtime-read-verifier-0"]["data"].update(
            consumer_ref="controller-consumer"), "read_binding")

    def test_principal_mapping_digest_is_not_just_a_label(self):
        self.reject(lambda r, s: s["authority-source"]["data"].update(
            principal_mapping_sha256="a" * 64), "principal_mapping")

    def test_reviewed_scope_covers_code_policy_resource_and_plan_claims(self):
        for group, field, value in (("scope", "pr", "2"),
                ("build", "image_digest", "sha256:" + "a" * 64),
                ("authority", "epoch", "8"), ("attempt", "enforcement_epoch", "10")):
            with self.subTest(group=group, field=field):
                self.reject(lambda r, s: r[group].update({field: value}))
        self.reject(lambda r, s: r["resources"]["entries"][0].update(maximum_count=100),
                    "authority_scope")
        self.reject(lambda r, s: r["fixtures"].update(interaction_plan_ref="other-plan"),
                    "authority_scope")
        self.reject(lambda r, s: s["runtime-authority-scope"]["data"].update(epoch="8"),
                    "authority_scope")
        self.reject(lambda r, s: r["authority"].update(scope_sha256="a" * 64), "authority_scope")

    def test_manifest_and_document_bytes_cannot_be_substituted(self):
        for ref in ("implementation-ref", "inventory-ref", "policy-ref", "plan-ref", "contract-ref",
                    "identity-map-ref"):
            with self.subTest(ref=ref):
                self.reject(lambda r, s: s[ref]["data"].update(sha256="0" * 64), "reference_binding")

    def test_runtime_decision_sidecar_binds_exact_decision_and_security_round(self):
        for field, value in (("security_round", 1), ("source_ref", "copied-source"),
                             ("generation", "9")):
            with self.subTest(field=field):
                self.reject(lambda r, s: s["runtime-qa"]["data"].update({field: value}))
        for field, value in (("round", 3), ("participant_id", "0" * 8 +
                             "-0000-4000-8000-000000000000")):
            with self.subTest(field=field):
                self.reject(lambda r, s: s["runtime-qa"]["data"]["claim"].update({field: value}),
                            "decision_binding")

    def test_decision_stage_and_participant_are_bound_beyond_role_names(self):
        for field in ("stage_id", "participant_id"):
            def mutate(r, s):
                r["authority"]["decisions"][0][field] = "0" * 8 + "-0000-4000-8000-000000000000"
                s["runtime-security"]["data"]["claim"] = copy.deepcopy(r["authority"]["decisions"][0])
            with self.subTest(field=field):
                self.reject(mutate, "decision_stage")
        self.reject(lambda r, s: s["installation-approval-0"]["data"].update(security_round=1),
                    "security_round_binding")

    def test_scope_pins_source_mapping_and_both_consumer_definitions(self):
        self.reject(lambda r, s: s["verifier-consumer"]["data"].update(
            provisioning_ref="replacement-provisioning"), "authority_scope")
        self.reject(lambda r, s: s["authority-source"]["data"]["stage_bindings"][0].update(
            principal_ref="principal-executor"), "authority_scope")
        self.reject(lambda r, s: s["authority-source"]["data"]["stage_bindings"][0].update(
            purpose="installation"), "stage_mapping")

    def test_self_review_shared_reviewer_and_qa_before_security_refuse(self):
        mutations = [
            lambda r, s: r["authority"]["decisions"][0].update(principal=r["authority"]["author"]),
            lambda r, s: r["authority"]["decisions"][1].update(
                principal=r["authority"]["decisions"][0]["principal"]),
            lambda r, s: r["authority"]["decisions"].reverse(),
            lambda r, s: r["authority"]["decisions"][1].update(decided_at="2025-12-31T23:40:00Z"),
        ]
        for mutate in mutations:
            with self.subTest(mutate=mutate):
                self.reject(mutate)

    def test_checkpoints_are_complete_ordered_and_unique(self):
        for mutate in (
            lambda r, s: r["attempt"]["checkpoints"].pop(3),
            lambda r, s: r["attempt"]["checkpoints"][0].update(phase="setup"),
            lambda r, s: r["attempt"]["checkpoints"][1].update(ref="checkpoint-0"),
            lambda r, s: r["attempt"]["checkpoints"][1].update(checked_at=at("00:04:00")),
            lambda r, s: r["attempt"]["checkpoints"][-1].update(checked_at=at("00:23:59")),
            lambda r, s: r["rollout"].update(started_at=at("00:02:59")),
        ):
            with self.subTest(mutate=mutate):
                self.reject(mutate)

    def test_final_acceptance_rechecks_freshness_against_actual_clock(self):
        r, s, now = authority_fixture()
        with self.assertRaisesRegex(Refusal, "stale_checkpoint"):
            self.check(r, s, now + timedelta(seconds=31))
        with self.assertRaisesRegex(Refusal, "expiry"):
            self.check(r, s, now + timedelta(minutes=16))
        with self.assertRaisesRegex(Refusal, "clock"):
            self.check(r, s, now.replace(tzinfo=None))

    def test_read_equality_requires_type_scope_epoch_and_effective_decisions(self):
        for change in ({"epoch": "8"}, {"resource_epoch": "10"},
                       {"scope_sha256": "1" * 64}, {"source_ref": "other-source"},
                       {"consumer": "controller"}, {"effective_decision_refs": ["runtime-qa", "runtime-security"]},
                       {"read_at": at("00:00:00")}, {"ref": "aliased-read"}):
            with self.subTest(change=change):
                self.reject(lambda r, s: s["runtime-read-verifier-1"]["data"]["claim"].update(change),
                            "read_binding")

    def test_unused_and_reused_runtime_read_claims_refuse(self):
        def extra(r, s):
            read = copy.deepcopy(r["authority"]["reads"][0])
            read["ref"] = "unused-read"
            r["authority"]["reads"].append(read)
        self.reject(extra, "unreferenced_authority_read")
        self.reject(lambda r, s: r["attempt"]["checkpoints"][1].update(
            controller_read_ref="runtime-read-controller-0"))

    def test_current_history_is_not_an_older_same_head_snapshot(self):
        self.reject(lambda r, s: s["authority-source"]["data"].update(
            history_ref="runtime-history-4"), "authority_scope")

    def test_history_must_be_complete_contiguous_and_bound_to_the_read(self):
        for changed in ({"coverage": "gap"}, {"coverage": "unknown"}, {"first_sequence": 2},
                        {"last_sequence": 2}, {"generation": "9"}, {"epoch": "8"},
                        {"ended_at": at("00:00:09")},
                        {"effective_decision_refs": ["runtime-qa", "runtime-security"]}):
            def mutate(r, s):
                s["runtime-history-0"]["data"].update(changed)
                rehash_history(r, s, "runtime-history-0")
            with self.subTest(changed=changed):
                self.reject(mutate)
        self.reject(lambda r, s: s["runtime-history-0"]["data"].update(generation="9"),
                    "authority_history")

    def test_push_then_revert_does_not_restore_spent_approval(self):
        def replay(r, s):
            h = s["runtime-history-5"]["data"]
            h["events"].extend([
                {"sequence": 2, "kind": "push", "observed_at": at("00:10:00"),
                 "scope_sha256": "1" * 64, "invalidated_decision_refs": []},
                {"sequence": 3, "kind": "push", "observed_at": at("00:11:00"),
                 "scope_sha256": r["authority"]["scope_sha256"], "invalidated_decision_refs": []}])
            h["last_sequence"] = 3
            rehash_history(r, s, "runtime-history-5")
        self.reject(replay, "invalidated_authority")

    def test_revocation_dismissal_and_scope_changes_never_fall_back(self):
        for kind in ("revoke", "dismiss", "scope_change", "policy_change", "resource_change"):
            def mutate(r, s):
                event = s["runtime-history-5"]["data"]["events"][0]
                event.update(kind=kind, observed_at=at("00:12:00"))
                rehash_history(r, s, "runtime-history-5")
            with self.subTest(kind=kind):
                self.reject(mutate, "invalidated_authority")
        def invalidated(r, s):
            s["runtime-history-0"]["data"]["events"][0]["invalidated_decision_refs"] = ["runtime-security"]
            rehash_history(r, s, "runtime-history-0")
        self.reject(invalidated, "invalidated_authority")

    def test_coherently_rehashed_truncated_or_rewritten_history_still_refuses(self):
        for change in ({"events": [], "last_sequence": 0},
                       {"started_at": "2025-12-31T23:10:00Z"}):
            def mutate(r, s):
                s["runtime-history-5"]["data"].update(change)
                rehash_history(r, s, "runtime-history-5")
            with self.subTest(change=change):
                self.reject(mutate, "authority_history_rewrite")
        def rewrite(r, s):
            s["runtime-history-5"]["data"]["events"][0]["scope_sha256"] = "2" * 64
            rehash_history(r, s, "runtime-history-5")
        self.reject(rewrite, "authority_history_rewrite")

    def test_nonce_consumption_is_attempt_owner_epoch_and_time_bound(self):
        for change in ({"outcome": "replayed"}, {"outcome": "unknown"},
                       {"run": {"run_id": "9002", "attempt": 1, "control_sha": "c" * 40}},
                       {"nonce_ref": "old-nonce"}, {"owner_ref": "principal-caller"},
                       {"authority_epoch": "8"}, {"resource_epoch": "10"},
                       {"execution_ref": "other-execution"}, {"consumed_at": at("00:00:11")}):
            with self.subTest(change=change):
                self.reject(lambda r, s: s["consumption-ref"]["data"].update(change))

    def test_execution_grant_cannot_reuse_installation_scope_or_other_attempt(self):
        for change in ({"generation": "9"}, {"epoch": "8"}, {"resource_epoch": "10"},
                       {"scope_sha256": "a" * 64}, {"installation_approval_refs": ["installation-approval-0"]}):
            with self.subTest(change=change):
                self.reject(lambda r, s: s["execution-ref"]["data"].update(change), "execution_binding")
        self.reject(lambda r, s: s["execution-ref"]["data"]["claim"]["run"].update(attempt=2),
                    "execution_binding")

    def test_every_fence_is_current_atomic_exact_phase_and_nonce(self):
        for change in ({"boundary": "application_flag"}, {"boundary": "unknown"},
                       {"phase": "setup"}, {"checkpoint_ref": "checkpoint-1"},
                       {"resource_epoch": "10"}, {"authority_epoch": "8"},
                       {"nonce_ref": "other-nonce"}, {"consumption_ref": "other-consumption"},
                       {"observed_at": at("00:00:09")}, {"valid_until": at("00:00:10")},
                       {"valid_until": at("00:01:11")}):
            with self.subTest(change=change):
                self.reject(lambda r, s: s["fence-0"]["data"].update(change))
        self.reject(lambda r, s: r["attempt"]["checkpoints"][1].update(mutation_fence_ref="fence-0"),
                    "mutation_fence")

    def test_installation_is_separate_fresh_approved_and_exactly_mapped(self):
        for change in ({"checked_at": at("00:00:00")}, {"mapping_ref": "other-mapping"},
                       {"source_ref": "other-source"}, {"scope_sha256": "a" * 64},
                       {"controller_read_ref": "runtime-read-controller-0"}):
            with self.subTest(change=change):
                self.reject(lambda r, s: s["installation-approval-0"]["data"].update(change))
        self.reject(lambda r, s: s["installation-approval-0"]["data"]["decisions"].reverse(),
                    "decision_order")
        self.reject(lambda r, s: s["installation-approval-0"]["data"]["decisions"][0].update(
            principal=r["authority"]["executor"]), "decision_scope")
        self.reject(lambda r, s: s["install-read-verifier-0"]["data"].update(
            consumer_ref="controller-consumer"), "read_binding")

    def test_rollout_and_effects_cannot_outlive_their_checkpoint_fences(self):
        self.reject(lambda r, s: r["rollout"].update(started_at=at("00:03:31")),
                    "mutation_without_checkpoint")
        def late_effect(r, s):
            for ledger in ("expected_effects", "actual_effects"):
                r["observations"][ledger][0]["observed_at"] = at("00:08:31")
        self.reject(late_effect, "mutation_without_checkpoint")

    def test_installation_decisions_do_not_change_behind_one_epoch(self):
        def changed(r, s):
            install = s["installation-approval-5"]["data"]
            install["decisions"][1]["round"] = 3
        self.reject(changed, "installation_decision_drift")

    def test_effective_qa_decision_must_exist_at_runtime_and_installation_read(self):
        for prefix in ("runtime", "install"):
            def mutate(r, s):
                if prefix == "runtime":
                    r["authority"]["decisions"][1]["decided_at"] = at("00:00:09")
                    s["runtime-qa"]["data"]["claim"] = copy.deepcopy(r["authority"]["decisions"][1])
                else:
                    for i in range(6):
                        s["installation-approval-" + str(i)]["data"]["decisions"][1]["decided_at"] = at("00:00:09")
                for role in ("controller", "verifier"):
                    ref = prefix + "-read-" + role + "-0"
                    s[ref]["data"]["claim"]["read_at"] = at("00:00:08")
                    for read in r["authority"]["reads"]:
                        if read["ref"] == ref:
                            read["read_at"] = at("00:00:08")
                ref = prefix + "-history-0"
                s[ref]["data"]["ended_at"] = at("00:00:08")
                rehash_history(r, s, ref)
            with self.subTest(prefix=prefix):
                self.reject(mutate, "authority_history_time")

    def test_final_installation_read_is_fresh_at_verification_not_only_checkpoint(self):
        def mutate(r, s):
            for role in ("controller", "verifier"):
                s["install-read-" + role + "-5"]["data"]["claim"]["read_at"] = at("00:23:00")
            s["install-history-5"]["data"]["ended_at"] = at("00:23:00")
            rehash_history(r, s, "install-history-5")
        self.reject(mutate, "stale_read")

    def test_final_installation_read_at_exact_sixty_second_age_is_consistent(self):
        r, s, now = authority_fixture()
        for role in ("controller", "verifier"):
            s["install-read-" + role + "-5"]["data"]["claim"]["read_at"] = at("00:23:30")
        s["install-history-5"]["data"]["ended_at"] = at("00:23:30")
        rehash_history(r, s, "install-history-5")
        self.assertIsNone(self.check(r, s, now))

    def test_qa_decision_at_exact_read_time_is_consistent(self):
        r, s, now = authority_fixture()
        r["authority"]["decisions"][1]["decided_at"] = at("00:00:10")
        s["runtime-qa"]["data"]["claim"] = copy.deepcopy(r["authority"]["decisions"][1])
        for i in range(6):
            s["installation-approval-" + str(i)]["data"]["decisions"][1]["decided_at"] = at("00:00:10")
        self.assertIsNone(self.check(r, s, now))

    def test_later_history_cannot_backdate_an_event_missing_from_complete_snapshot(self):
        for prefix in ("runtime", "install"):
            def mutate(r, s):
                ref = prefix + "-history-5"
                history = s[ref]["data"]
                history["events"].append({"sequence": 2, "kind": "push",
                    "observed_at": "2025-12-31T23:40:30Z", "scope_sha256": "1" * 64,
                    "invalidated_decision_refs": ["other-spent-decision"]})
                history["last_sequence"] = 2
                rehash_history(r, s, ref)
            with self.subTest(prefix=prefix):
                self.reject(mutate, "authority_history_backfill")

    def test_constant_refusals_do_not_expose_rejected_authority_data(self):
        r, s, now = authority_fixture()
        s["authority-source"]["data"]["private_error"] = "private-synthetic-sentinel"
        with self.assertRaises(Refusal) as raised:
            self.check(r, s, now)
        self.assertNotIn("private-synthetic-sentinel", str(raised.exception))
        self.assertNotIn("private-synthetic-sentinel", repr(raised.exception))


if __name__ == "__main__":
    unittest.main()
