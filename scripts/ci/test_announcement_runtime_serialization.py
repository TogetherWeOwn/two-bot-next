"""Closed fabricated serialization graphs, never an actual all-writer fence.

Claims are explicitly repinned to reach semantic predicates. Actions is the
suite runner; these tests perform no locking, network, mutation or deployment.
"""

import copy
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from announcement_authority_fixtures import principal
from announcement_provenance_fixtures import at, envelope, evidence_index
from announcement_serialization_fixtures import repin_serialization, serialization_fixture
from announcement_runtime_authority import check_authority_graph_claims
from announcement_runtime_evidence import SERIALIZATION_RECORDS
from announcement_runtime_policy import runtime_preflight
from announcement_runtime_schema import Refusal
from announcement_runtime_serialization import check_serialization_claims


def plan(records):
    return records["runtime-serialization-plan"]["data"]


def node(records, ref):
    return next(n for n in plan(records)["nodes"] if n["ref"] == ref)


def witness(records, ref):
    return records[node(records, ref)["witness_ref"]]["data"]


def lock(receipt, name):
    return next(l for l in receipt["attempt"]["locks"] if l["name"] == name)


def sync_locks(receipt, records):
    for claim in receipt["attempt"]["locks"]:
        records[claim["ref"]]["data"]["claim"] = copy.deepcopy(claim)


def add_writer(receipt, records, *, parent="leaf", acquire=None):
    """A separately declared control job (e.g. a standalone synthetic writer)."""
    receipt["attempt"]["job_refs"].append("job-extra-writer")
    records["job-extra-writer"] = envelope("job-extra-writer", "job", {
        **records["job-lifecycle"]["data"], "id": "91"})
    plan(records)["nodes"].append({"ref": "extra-writer", "job_ref": "job-extra-writer",
        "parent_refs": [parent], "role": "writer", "acquires": acquire or [],
        "witness_ref": "witness-extra-writer"})
    plan(records)["writer_refs"].append("extra-writer")
    records["witness-extra-writer"] = envelope("witness-extra-writer", "serialization_participant", {
        **witness(records, "leaf"), "node_ref": "extra-writer", "job_ref": "job-extra-writer"})


class Serialization(unittest.TestCase):
    def check(self, receipt, records, now):
        return check_serialization_claims(receipt, evidence_index(records), now)

    def reject(self, mutate, code=None, *, repin=True):
        receipt, records, now = serialization_fixture()
        mutate(receipt, records)
        if repin:
            repin_serialization(receipt, records)
        with self.assertRaisesRegex(Refusal, code or "announcement evidence refused"):
            self.check(receipt, records, now)


class Graph(Serialization):
    def test_consistent_graph_is_not_installed_serialization(self):
        receipt, records, now = serialization_fixture()
        index = evidence_index(records)
        self.assertIsNone(check_serialization_claims(receipt, index, now))
        self.assertIsNone(check_authority_graph_claims(receipt, index, now))
        index.require_all_used()
        with self.assertRaisesRegex(Refusal, "unsupported"):
            runtime_preflight()

    def test_writer_can_inherit_distinct_parent_and_leaf_locks(self):
        receipt, records, now = serialization_fixture()
        add_writer(receipt, records)
        repin_serialization(receipt, records)
        self.assertIsNone(self.check(receipt, records, now))

    def test_plan_cannot_escape_authority_resource_scope(self):
        self.reject(lambda r, s: node(s, "leaf").update(ref="changed-leaf"),
                    "serialization_scope", repin=False)

    def test_inventory_manifest_must_match(self):
        self.reject(lambda r, s: plan(s)["inventory_manifest"].update(sha256="0" * 64),
                    "serialization_inventory")

    def test_duplicate_node_is_rejected(self):
        self.reject(lambda r, s: plan(s)["nodes"].append(copy.deepcopy(node(s, "leaf"))),
                    "serialization_nodes")

    def test_every_declared_job_requires_one_node(self):
        self.reject(lambda r, s: plan(s)["nodes"].pop(), "serialization_jobs")

    def test_one_job_cannot_impersonate_two_nodes(self):
        self.reject(lambda r, s: node(s, "leaf").update(job_ref="job-control"),
                    "serialization_jobs")

    def test_writer_inventory_is_exact(self):
        for refs in ([], ["leaf", "leaf"], ["leaf", "build"]):
            with self.subTest(refs=refs):
                self.reject(lambda r, s: plan(s).update(writer_refs=refs))

    def test_unknown_parent_is_rejected(self):
        self.reject(lambda r, s: node(s, "leaf").update(parent_refs=["missing"]),
                    "serialization_cycle")

    def test_self_and_multi_node_cycles_are_rejected(self):
        def cycle(r, s):
            node(s, "leaf")["parent_refs"] = ["build"]
            node(s, "build")["parent_refs"] = ["leaf"]
        self.reject(cycle, "serialization_cycle")
        self.reject(lambda r, s: node(s, "leaf").update(parent_refs=["leaf"]),
                    "serialization_cycle")

    def test_disconnected_second_root_is_rejected(self):
        self.reject(lambda r, s: node(s, "leaf").update(parent_refs=[]), "serialization_root")

    def test_only_one_orchestrator_is_permitted(self):
        self.reject(lambda r, s: node(s, "build").update(role="orchestrator"),
                    "serialization_root")

    def test_parent_cannot_hold_child_identical_lock(self):
        self.reject(lambda r, s: node(s, "controller").update(
            acquires=["deploy-staging", "staging-lifecycle"]), "serialization_root")
        self.reject(lambda r, s: node(s, "leaf").update(
            acquires=["deploy-staging", "staging-lifecycle"]), "serialization_nested_lock")

    def test_descendant_cannot_reacquire_held_leaf(self):
        self.reject(lambda r, s: add_writer(r, s, acquire=["staging-lifecycle"]),
                    "serialization_nested_lock")

    def test_parallel_second_leaf_is_unsupported(self):
        self.reject(lambda r, s: add_writer(r, s, parent="controller", acquire=["staging-lifecycle"]),
                    "serialization_reacquire")

    def test_standalone_writer_cannot_bypass_lifecycle_leaf(self):
        self.reject(lambda r, s: add_writer(r, s, parent="controller"),
                    "serialization_writer_fence")

    def test_observer_or_candidate_cannot_acquire_locks(self):
        for ref in ("build", "external-observer"):
            with self.subTest(ref=ref):
                self.reject(lambda r, s: node(s, ref).update(acquires=["staging-lifecycle"]),
                            "serialization_job_role")

    def test_writer_cannot_relabel_itself_as_observer_or_build(self):
        for role in ("observer", "credential_free_build"):
            with self.subTest(role=role):
                self.reject(lambda r, s: node(s, "leaf").update(role=role),
                            "serialization_writers")

    def test_noncontroller_cannot_claim_writer_principal(self):
        self.reject(lambda r, s: s["producer-controller"]["data"].update(
            principal=principal("executor")), "serialization_job_role")


class Witnesses(Serialization):
    def test_missing_witness_is_rejected(self):
        self.reject(lambda r, s: s.pop("witness-leaf"), "unresolved_reference")

    def test_copied_participant_is_rejected(self):
        self.reject(lambda r, s: node(s, "leaf").update(witness_ref="witness-controller"),
                    "serialization_participant")

    def test_wrong_participant_time_job_or_lock_is_rejected(self):
        for change in ({"started_at": at("00:00:01")}, {"job_ref": "job-build"},
                       {"held_lock_refs": ["lock-staging-lifecycle", "lock-deploy-staging"]},
                       {"held_lock_refs": []}):
            with self.subTest(change=change):
                self.reject(lambda r, s: witness(s, "leaf").update(change),
                            "serialization_participant")

    def test_interference_or_unknown_is_never_success(self):
        for change in ({"outcome": "unknown"}, {"outcome": "interference"},
                       {"other_writer_refs": ["canonical-writer"]}):
            with self.subTest(change=change):
                self.reject(lambda r, s: witness(s, "leaf").update(change),
                            "serialization_interference")

    def test_controller_cannot_observe_serialization(self):
        def mutate(r, s):
            s["controller-observer"] = envelope("controller-observer", "observer", {
                "principal": principal("controller"), "job_ref": "job-control",
                "provisioning_ref": "controller-provisioning"})
            witness(s, "leaf")["observer_ref"] = "controller-observer"
        self.reject(mutate, "serialization_independence")

    def test_candidate_producer_cannot_observe_own_serialization(self):
        self.reject(lambda r, s: s["producer-ref"]["data"].update(
            principal=principal("verifier")), "serialization_independence")

    def test_observer_must_span_full_attempt_not_just_leaf(self):
        def mutate(r, s):
            r["attempt"]["job_refs"].append("job-short-observer")
            s["job-short-observer"] = envelope("job-short-observer", "job", {
                **s["job-resource-observer"]["data"], "id": "92", "started_at": at("00:00:01")})
            s["short-observer"] = envelope("short-observer", "observer", {
                **s["resource-observer"]["data"], "job_ref": "job-short-observer"})
            plan(s)["nodes"].append({"ref": "short-observer", "job_ref": "job-short-observer",
                "parent_refs": ["controller"], "role": "observer", "acquires": [],
                "witness_ref": "witness-short-observer"})
            s["witness-short-observer"] = envelope("witness-short-observer", "serialization_participant", {
                **witness(s, "resource-observer"), "node_ref": "short-observer",
                "job_ref": "job-short-observer", "started_at": at("00:00:01")})
            witness(s, "leaf")["observer_ref"] = "short-observer"
        self.reject(mutate, "serialization_observer")

    def test_observer_provisioning_cannot_drift(self):
        def mutate(r, s):
            s["different-observer"] = copy.deepcopy(s["resource-observer"])
            s["different-observer"]["ref"] = "different-observer"
            s["different-observer"]["data"]["provisioning_ref"] = "caller-provisioning"
            witness(s, "leaf")["observer_ref"] = "different-observer"
        self.reject(mutate, "serialization_observer")

    def test_future_or_mixed_attempt_sidecars_refuse(self):
        self.reject(lambda r, s: s["witness-leaf"]["run"].update(attempt=2), "evidence_attempt")
        self.reject(lambda r, s: witness(s, "leaf").update(ended_at=at("00:25:00")),
                    "serialization_participant")


class Locks(Serialization):
    def test_parent_must_cover_start_through_restoration(self):
        for changes in ({"acquired_at": at("00:00:01")}, {"released_at": at("00:23:59")}):
            with self.subTest(changes=changes):
                def mutate(r, s):
                    lock(r, "deploy-staging").update(changes)
                    sync_locks(r, s)
                self.reject(mutate, "serialization_lock_window")

    def test_missing_or_aliased_lock_name_refuses(self):
        self.reject(lambda r, s: lock(r, "staging-lifecycle").update(name="deploy-staging"),
                    "serialization_locks")
        self.reject(lambda r, s: lock(r, "staging-lifecycle").update(ref="lock-deploy-staging"),
                    "serialization_locks")

    def test_lock_epoch_owner_attempt_and_interference_are_bound(self):
        for change in ({"fence": "10"}, {"owner_ref": "principal-executor"},
                       {"interference_refs": ["canonical-deploy"]}):
            with self.subTest(change=change):
                def mutate(r, s):
                    lock(r, "staging-lifecycle").update(change)
                    sync_locks(r, s)
                self.reject(mutate, "serialization_fence")

    def test_lock_claim_and_acquiring_node_cannot_be_substituted(self):
        for change in ({"node_ref": "build"}, {"boundary": "workflow_only"},
                       {"boundary": "application_only"}, {"boundary": "unknown"}):
            with self.subTest(change=change):
                self.reject(lambda r, s: s["lock-staging-lifecycle"]["data"].update(change),
                            "serialization_lock_binding")

    def test_lock_cannot_outlive_acquiring_job(self):
        def mutate(r, s):
            s["job-lifecycle"]["data"].update(started_at=at("00:01:00"), ended_at=at("00:23:00"))
            witness(s, "leaf").update(started_at=at("00:01:00"), ended_at=at("00:23:00"))
        self.reject(mutate, "serialization_lock_window")

    def test_child_must_fit_parent_window_even_without_own_lock(self):
        def mutate(r, s):
            s["job-lifecycle"]["data"]["ended_at"] = at("00:23:00")
            witness(s, "leaf")["ended_at"] = at("00:23:00")
            lock(r, "staging-lifecycle")["released_at"] = at("00:23:00")
            sync_locks(r, s)
            node(s, "resource-observer")["parent_refs"] = ["leaf"]
            witness(s, "resource-observer")["held_lock_refs"].append("lock-staging-lifecycle")
        self.reject(mutate, "serialization_parent_window")

    def test_unknown_keys_in_every_serialization_kind_refuse(self):
        for kind in SERIALIZATION_RECORDS:
            with self.subTest(kind=kind):
                def mutate(r, s):
                    next(v for v in s.values() if v["kind"] == kind)["data"]["extension"] = True
                self.reject(mutate, "fields")


if __name__ == "__main__":
    unittest.main()
