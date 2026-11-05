"""Hermetic work-drain adversaries; not a drain, fence, quiescence or recovery proof.

Passing here shows only that a fabricated, unauthenticated work inventory,
fence claim, drain claim, three all-zero snapshots and a tiny telemetry window
claim agree with the narrow lifecycle fixture. It is deliberately PARTIAL: the
restoration authority, verification, postcondition, readiness, snapshot,
compatibility and artifact references have no sidecars here and are not
consumed by the checker, so a complete packet must still refuse them. Inventory
completeness, observer authentication and real cancellation are not shown. The
checker is not patched or mocked, returns no capability, and runtime_preflight
stays unsupported.
"""

import copy
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from announcement_drain_fixtures import (
    INVENTORY, NOW, OBSERVER, SNAPSHOTS, SURFACES, drain_fixture, principal, repin_drain,
)
from announcement_lifecycle_fixtures import CLEANUP, READ, effect
from announcement_provenance_fixtures import at, envelope, evidence_index
from announcement_runtime_drain import check_drain_claims
from announcement_runtime_evidence import WORK_RECORDS
from announcement_runtime_policy import runtime_preflight
from announcement_runtime_schema import Refusal

UNRESOLVED = ("restoration-authority-ref", "restoration-verification-ref", "postcondition-ref",
              "readiness-after-ref", "snapshot-ref", "compatibility-ref", "artifact-ref")
CATEGORIES = ("queued", "in_flight", "claimed", "delayed")


def data(records, ref):
    return records[ref]["data"]


def effects(receipt):
    return receipt["observations"]["actual_effects"]


def retime_cleanup(receipt, records, clock):
    """Move the cleanup write and its sidecars together; the drain stays put."""
    next(x for x in effects(receipt) if x["operation_id"] == CLEANUP)["observed_at"] = at(clock)
    receipt["lifecycle"]["cleanup"][0]["observed_at"] = at(clock)
    data(records, "transition-" + CLEANUP)["claim"]["observed_at"] = at(clock)
    data(records, "state-final")["observed_at"] = at(clock)


def add_effect(receipt, records, method, clock, operation="op-late"):
    effects(receipt).append(effect(operation, len(effects(receipt)) + 1, method, clock,
                                   transition=False))
    repin_drain(receipt, records)


def add_writer_job(receipt, records, subject_digit="f"):
    """Add a second, non-observer job whose producer has the given subject."""
    receipt["attempt"]["job_refs"].append("job-writer")
    receipt["attempt"]["producer_refs"].append("producer-writer")
    records["producer-writer"] = envelope("producer-writer", "producer", {
        "principal": principal("principal-writer", subject_digit),
        "workflow": copy.deepcopy(receipt["control"]["workflow"]),
        "repository": "TogetherWeOwn/two-bot-next"})
    job = copy.deepcopy(data(records, "job-ref"))
    job.update(id="78", producer_ref="producer-writer", source="candidate")
    records["job-writer"] = envelope("job-writer", "job", job)


class Drain(unittest.TestCase):
    def check(self, receipt, records, now=NOW):
        return check_drain_claims(receipt, evidence_index(records), now)

    def accept(self, mutate=None, prior="absent"):
        receipt, records, now = drain_fixture(prior)
        if mutate:
            mutate(receipt, records)
        self.assertIsNone(self.check(receipt, records, now))

    def reject(self, mutate, code, prior="absent", repin=True):
        receipt, records, now = drain_fixture(prior)
        mutate(receipt, records)
        if repin:
            repin_drain(receipt, records)
        with self.assertRaisesRegex(Refusal, "announcement evidence refused: " + code + "$"):
            self.check(receipt, records, now)

    def reject_each(self, ref, changes, code):
        for field, value in changes.items():
            with self.subTest(ref=ref, field=field):
                self.reject(lambda r, s: data(s, ref).update({field: value}), code, repin=False)


class Coherence(Drain):
    def test_minimal_drain_is_coherent_and_grants_no_capability(self):
        for prior in ("absent", "present"):
            receipt, records, now = drain_fixture(prior)
            self.assertIsNone(self.check(receipt, records, now))
            self.assertFalse(receipt["promotable"])
        with self.assertRaisesRegex(Refusal, "unsupported"):
            runtime_preflight()

    def test_unresolved_recovery_and_postcondition_references_are_retained(self):
        receipt, records, _ = drain_fixture()
        life = receipt["lifecycle"]
        held = {life[k] for k in life if isinstance(life[k], str)}
        held.update(life["postcondition_refs"], life["restorable_artifact_refs"])
        self.assertTrue(set(UNRESOLVED) <= held)
        self.assertFalse(set(UNRESOLVED) & set(records))
        self.accept()

    def test_every_sidecar_is_resolved_by_the_narrow_check(self):
        # Accounting for these narrow records only; NOT full packet verification.
        for prior in ("absent", "present"):
            receipt, records, now = drain_fixture(prior)
            index = evidence_index(records)
            check_drain_claims(receipt, index, now)
            index.require_all_used()

    def test_extra_well_formed_sidecar_is_preserved_as_unreferenced(self):
        receipt, records, now = drain_fixture()
        records["snapshot-extra"] = copy.deepcopy(records["queue-final-ref"])
        records["snapshot-extra"]["ref"] = "snapshot-extra"
        index = evidence_index(records)
        self.assertIsNone(check_drain_claims(receipt, index, now))
        with self.assertRaisesRegex(Refusal, "unreferenced_evidence$"):
            index.require_all_used()

    def test_a_distinct_writer_job_is_coherent(self):
        self.accept(lambda r, s: add_writer_job(r, s, "9"))

    def test_lifecycle_refusals_precede_drain_claims(self):
        self.reject(lambda r, _: r["lifecycle"].update(abort="fenced"), "lifecycle_adverse",
                    repin=False)
        self.reject(lambda r, _: effects(r)[3].update(sequence=5), "sequence_gap", repin=False)


class Intake(Drain):
    def test_work_sidecar_kinds_are_closed_and_nonempty(self):
        examples = {"work_inventory_content": INVENTORY, "work_snapshot": SNAPSHOTS[0],
                    "work_fence": "work-fence-ref", "work_drain": "drain-ref"}
        self.assertEqual(set(examples), set(WORK_RECORDS))
        for kind, ref in examples.items():
            _, records, _ = drain_fixture()
            for value in ({}, {**records[ref]["data"], "extra": True}, None):
                with self.subTest(kind=kind, data=type(value)):
                    changed = copy.deepcopy(records)
                    changed[ref]["data"] = copy.deepcopy(value)
                    with self.assertRaises(Refusal):
                        evidence_index(changed)

    def test_counts_must_be_nonnegative_integers_at_intake(self):
        for value in (-1, True, "0", None):
            with self.subTest(value=value):
                _, records, _ = drain_fixture()
                data(records, SNAPSHOTS[0])["surfaces"][0]["queued"] = value
                with self.assertRaisesRegex(Refusal, "integer$"):
                    evidence_index(records)

    def test_empty_surface_lists_refuse_at_intake(self):
        for ref, field in ((INVENTORY, "surfaces"), (SNAPSHOTS[1], "surfaces")):
            with self.subTest(ref=ref):
                _, records, _ = drain_fixture()
                data(records, ref)[field] = []
                with self.assertRaisesRegex(Refusal, "array_size$"):
                    evidence_index(records)

    def test_unknown_enum_values_and_bad_paths_refuse_at_intake(self):
        cases = ((INVENTORY, lambda d: d["surfaces"][0].update(kind="other"), "enum"),
                 (INVENTORY, lambda d: d["surfaces"][0].update(disposition="maybe"), "enum"),
                 (INVENTORY, lambda d: d["surfaces"][0].update(path="src/../x.rs"), "path"),
                 (INVENTORY, lambda d: d["surfaces"][0].update(path="src/./x.rs"), "path"),
                 (INVENTORY, lambda d: d["surfaces"][0].update(ref="Surface_Upper"),
                  "text_format"),
                 (SNAPSHOTS[0], lambda d: d.update(outcome="partial"), "enum"),
                 ("work-fence-ref", lambda d: d.update(boundary="other"), "enum"),
                 ("drain-ref", lambda d: d.update(outcome="other"), "enum"))
        for ref, mutate, code in cases:
            with self.subTest(ref=ref, code=code):
                _, records, _ = drain_fixture()
                mutate(data(records, ref))
                with self.assertRaisesRegex(Refusal, code + "$"):
                    evidence_index(records)

    def test_missing_wrong_kind_or_other_attempt_always_refuses(self):
        refs = (INVENTORY, "inventory-ref", "drain-ref", "work-fence-ref", "observer-ref",
                "job-ref", "producer-ref", "controller-consumer", "owner-ref", "coverage-ref",
                *SNAPSHOTS)
        for ref in refs:
            with self.subTest(ref=ref):
                self.reject(lambda r, s: s.pop(ref), "unresolved_reference", repin=False)
                _, records, _ = drain_fixture()
                records[ref]["run"]["attempt"] = 2
                with self.assertRaisesRegex(Refusal, "evidence_attempt$"):
                    evidence_index(records)

    def test_wrong_kind_reference_refuses(self):
        self.reject(lambda r, _: r["lifecycle"].update(drain_ref="work-fence-ref"),
                    "reference_kind", repin=False)
        self.reject(lambda r, _: r["lifecycle"].update(queue_baseline_ref="drain-ref"),
                    "reference_kind", repin=False)


class Binding(Drain):
    def test_wrong_manifest_refuses(self):
        self.reject(lambda r, _: r["resources"]["inventory_manifest"].update(sha256="0" * 64),
                    "reference_binding", repin=False)
        self.reject(lambda r, _: r["resources"]["inventory_manifest"].update(ref="nowhere"),
                    "unresolved_reference", repin=False)

        def agree_on_wrong_digest(receipt, records):
            receipt["resources"]["inventory_manifest"]["sha256"] = "0" * 64
            data(records, "inventory-ref")["sha256"] = "0" * 64
        self.reject(agree_on_wrong_digest, "drain_inventory_binding", repin=False)

    def test_wrong_source_candidate_or_control_refuses(self):
        for field, change in (("candidate", {"tree": "9" * 40}), ("control", {"commit": "9" * 40}),
                              ("source_manifest", {"revision": "9" * 40}),
                              ("source_manifest", {"ref": "other-manifest"})):
            with self.subTest(field=field, change=change):
                self.reject(lambda r, s: data(s, INVENTORY)[field].update(change),
                            "drain_inventory_binding")

    def test_duplicate_or_unknown_surfaces_refuse(self):
        def duplicate(receipt, records):
            surfaces = data(records, INVENTORY)["surfaces"]
            surfaces.append(copy.deepcopy(surfaces[0]))
        self.reject(duplicate, "drain_surface_inventory")

        def duplicate_in_snapshot(receipt, records):
            surfaces = data(records, SNAPSHOTS[1])["surfaces"]
            surfaces.append(copy.deepcopy(surfaces[0]))
        self.reject(duplicate_in_snapshot, "drain_surface_inventory")

        def unknown(receipt, records):
            data(records, SNAPSHOTS[1])["surfaces"][0]["surface_ref"] = "surface-other"
        self.reject(unknown, "drain_surface_inventory")

        def missing(receipt, records):
            data(records, SNAPSHOTS[2])["surfaces"].pop()
        self.reject(missing, "drain_surface_inventory")

        def extra(receipt, records):
            surfaces = data(records, SNAPSHOTS[0])["surfaces"]
            surfaces.append({**surfaces[0], "surface_ref": "surface-extra"})
        self.reject(extra, "drain_surface_inventory")

    def test_duplicate_snapshot_or_job_references_refuse(self):
        self.reject(lambda r, _: r["lifecycle"].update(queue_final_ref="queue-baseline-ref"),
                    "reference_duplicate", repin=False)
        self.reject(lambda r, _: r["attempt"]["job_refs"].append("job-ref"),
                    "reference_duplicate", repin=False)


class Counts(Drain):
    def test_positive_counts_refuse_in_every_snapshot_and_category(self):
        for ref in SNAPSHOTS:
            for category in CATEGORIES:
                for surface in range(len(SURFACES)):
                    with self.subTest(ref=ref, category=category, surface=surface):
                        self.reject(lambda r, s: data(s, ref)["surfaces"][surface].update(
                            {category: 1}), "drain_nonempty", repin=False)


class Fence(Drain):
    def test_wrong_fence_fields_refuse(self):
        self.reject_each("work-fence-ref", {
            "source_ref": "other-source", "execution_ref": "other-execution",
            "owner_ref": "other-owner", "nonce_ref": "other-nonce", "authority_epoch": "8",
            "resource_epoch": "8", "inventory_sha256": "0" * 64,
            "closed_at": at("00:19:59"), "held_until": at("00:29:59"),
            "boundary": "application_only", "outcome": "lost"}, "drain_fence")

    def test_receipt_execution_nonce_and_epochs_must_match_the_fence(self):
        self.reject(lambda r, _: r["authority"]["execution"].update(ref="other-execution"),
                    "drain_fence", repin=False)
        self.reject(lambda r, _: r["authority"]["execution"].update(nonce_ref="other-nonce"),
                    "drain_fence", repin=False)
        self.reject(lambda r, _: r["authority"].update(epoch="8"), "drain_fence", repin=False)
        self.reject(lambda r, _: r["attempt"].update(enforcement_epoch="8"), "drain_fence",
                    repin=False)

    def test_owner_must_be_the_controller_consumer(self):
        self.reject(lambda _, s: data(s, "controller-consumer").update(role="verifier"),
                    "drain_owner", repin=False)

    def test_unresolved_or_unknown_drain_refuses(self):
        self.reject_each("drain-ref", {
            "inventory_ref": "other-inventory", "outcome": "unknown",
            "unknown_operation_refs": ["op-unknown"]}, "drain_unresolved")


class Window(Drain):
    def test_drain_must_start_at_the_observation_cutoff(self):
        for clock in ("00:19:59", "00:20:01"):
            with self.subTest(clock=clock):
                self.reject(lambda _, s: data(s, "drain-ref").update(started_at=at(clock)),
                            "drain_window", repin=False)

    def test_drain_may_not_end_before_it_starts_or_after_cleanup(self):
        for clock in ("00:19:59", "00:20:31", "00:30:01"):
            with self.subTest(clock=clock):
                self.reject(lambda _, s: data(s, "drain-ref").update(ended_at=at(clock)),
                            "drain_window", repin=False)

    def test_cleanup_before_the_drain_ends_refuses(self):
        for prior in ("absent", "present"):
            with self.subTest(prior=prior):
                self.reject(lambda r, s: retime_cleanup(r, s, "00:20:05"), "drain_window",
                            prior=prior, repin=False)

    def test_snapshot_times_are_bound_to_their_boundaries(self):
        for ref, clock in ((SNAPSHOTS[0], "00:01:01"), (SNAPSHOTS[0], "00:20:00"),
                           (SNAPSHOTS[1], "00:20:09"), (SNAPSHOTS[1], "00:20:11"),
                           (SNAPSHOTS[2], "00:29:59")):
            with self.subTest(ref=ref, clock=clock):
                self.reject(lambda _, s: data(s, ref).update(observed_at=at(clock)),
                            "drain_snapshot", repin=False)

    def test_wrong_snapshot_binding_or_outcome_refuses(self):
        for ref in SNAPSHOTS:
            self.reject_each(ref, {"inventory_sha256": "0" * 64, "observer_ref": "other-observer",
                                   "enforcement_epoch": "8", "outcome": "unknown"},
                             "drain_snapshot")


class Coverage(Drain):
    def test_partial_or_mismatched_coverage_refuses(self):
        for field, value in (("ref", "other-coverage"), ("run", {
                                "run_id": "9001", "attempt": 1, "control_sha": "7" * 40}),
                             ("result", "gap"), ("result", "truncated"), ("result", "sampled"),
                             ("result", "unknown"), ("started_at", at("00:00:01")),
                             ("ended_at", at("00:29:59")),
                             ("source_inventory_sha256", "0" * 64),
                             ("enforcement_epoch", "8"), ("first_sequence", 2),
                             ("last_sequence", 3), ("last_sequence", 5)):
            with self.subTest(field=field, value=value):
                self.reject(lambda r, _: r["observations"]["coverage"][0].update({field: value}),
                            "drain_coverage", repin=False)

    def test_exactly_one_coverage_claim_is_required(self):
        self.reject(lambda r, _: r["observations"]["coverage"].append(
            copy.deepcopy(r["observations"]["coverage"][0])), "drain_coverage", repin=False)

    def test_window_sidecar_must_equal_the_receipt_claim_and_operations(self):
        self.reject(lambda _, s: data(s, "coverage-ref")["claim"].update(last_sequence=3),
                    "drain_coverage", repin=False)
        self.reject(lambda _, s: data(s, "coverage-ref")["operation_ids"].pop(),
                    "drain_coverage", repin=False)
        self.reject(lambda _, s: data(s, "coverage-ref")["operation_ids"].reverse(),
                    "drain_coverage", repin=False)
        self.reject(lambda _, s: data(s, "coverage-ref")["operation_ids"].append(
            "op-unlisted"), "drain_coverage", repin=False)


class LateOperations(Drain):
    def test_candidate_wire_or_database_reads_after_the_cutoff_refuse(self):
        for method, clock in (("feed_post", "00:20:40"), ("db_read", "00:20:40"),
                              ("db_read", "00:20:30"), ("interaction_ack", "00:25:00")):
            with self.subTest(method=method, clock=clock):
                self.reject(lambda r, s: add_effect(r, s, method, clock), "drain_late_operation",
                            repin=False)

    def test_reads_before_the_cutoff_remain_coherent(self):
        receipt, records, now = drain_fixture()
        read = next(x for x in effects(receipt) if x["operation_id"] == READ)
        self.assertLess(read["observed_at"], receipt["attempt"]["observation_ended_at"])
        self.assertIsNone(self.check(receipt, records, now))


class Observer(Drain):
    def test_wrong_observer_job_provenance_refuses(self):
        for field, value in (("source", "candidate"), ("source", "control"),
                             ("source", "independent_verifier"),
                             ("head", "9" * 40), ("conclusion", "failure"),
                             ("started_at", at("00:00:01")), ("ended_at", at("00:29:59")),
                             ("ended_at", at("00:40:01")), ("producer_ref", "other-producer")):
            with self.subTest(field=field, value=value):
                self.reject(lambda _, s: data(s, "job-ref").update({field: value}),
                            "drain_observer", repin=False)

    def test_observer_job_must_be_an_attempt_job_with_an_attempt_producer(self):
        self.reject(lambda _, s: data(s, "observer-ref").update(job_ref="job-other"),
                    "drain_observer", repin=False)
        self.reject(lambda r, _: r["attempt"].update(producer_refs=["producer-other"]),
                    "drain_observer", repin=False)

    def test_producer_must_match_the_observer_and_control_workflow(self):
        self.reject(lambda _, s: data(s, "producer-ref").update(
            principal=principal("principal-observer", "9")), "drain_observer", repin=False)

        def wrong_workflow(receipt, records):
            data(records, "producer-ref")["workflow"]["sha"] = "9" * 40
        self.reject(wrong_workflow, "drain_observer", repin=False)

    def test_observer_may_not_overlap_any_other_role(self):
        subject = principal("principal-observer", OBSERVER[0])["subject_id"]
        authority = ("author", "executor", "caller")
        for role in authority:
            with self.subTest(role=role):
                def overlap(receipt, records):
                    receipt["authority"][role]["subject_id"] = subject
                self.reject(overlap, "drain_independence", repin=False)

        def controller(receipt, records):
            data(records, "controller-consumer")["principal"]["subject_id"] = subject
        self.reject(controller, "drain_independence", repin=False)

        def owner(receipt, records):
            data(records, "owner-ref")["principal"]["subject_id"] = subject
        self.reject(owner, "drain_independence", repin=False)

    def test_observer_may_not_overlap_a_candidate_or_writer_job_producer(self):
        for source in ("candidate", "control", "independent_verifier"):
            with self.subTest(source=source):
                def overlap(receipt, records):
                    add_writer_job(receipt, records, OBSERVER[0])
                    data(records, "job-writer")["source"] = source
                self.reject(overlap, "drain_independence", repin=False)


if __name__ == "__main__":
    unittest.main()
