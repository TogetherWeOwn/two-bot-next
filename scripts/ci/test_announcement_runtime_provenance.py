"""Offline consistency tests for synthetic provenance claims.

A passing check here shows only that hand-written claims agree. It is no
authentication, root, qualification or live acceptance, and runtime_preflight
still refuses unconditionally.
"""

import copy
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import announcement_provenance_fixtures as fx
from announcement_runtime_evidence import EvidenceIndex, PROVENANCE_RECORDS, canonical_digest
from announcement_runtime_policy import runtime_preflight
from announcement_runtime_provenance import check_provenance_claims
from announcement_runtime_schema import Refusal

OTHER = "7" * 40
OTHER_RUN = {"run_id": "9001", "attempt": 2, "control_sha": fx.CONTROL_SHA}
SAMPLE_REFS = ("served-one", "served-two", "served-before", "served-after")


def data(sidecars, ref):
    return sidecars[ref]["data"]


def receipt_sample(receipt, ref):
    for sample in (*receipt["served"]["samples"], receipt["lifecycle"]["served_before"],
                   receipt["lifecycle"]["served_after"]):
        if sample["ref"] == ref:
            return sample
    raise KeyError(ref)


def set_sample(receipt, sidecars, ref, **changes):
    """Change a served sample coherently in the receipt and its external sidecar."""
    sample = receipt_sample(receipt, ref)
    sample.update(changes)
    data(sidecars, sample["external_source_ref"])["sample"] = copy.deepcopy(sample)


def set_worker(receipt, sidecars, mutate):
    """Change the candidate worker coherently in receipt, build sidecar, samples."""
    mutate(receipt["build"]["worker"])
    data(sidecars, "provenance-ref")["worker"] = copy.deepcopy(receipt["build"]["worker"])


def gate(receipt, name):
    return next(x for x in receipt["gates"]["records"] if x["name"] == name)


def set_gate_time(receipt, sidecars, name, when):
    gate(receipt, name)["observed_at"] = when
    data(sidecars, "gs-" + name)["observed_at"] = when


class Base(unittest.TestCase):
    def setUp(self):
        self.receipt, self.sidecars, self.now = fx.provenance_fixture()

    def check(self, run=None):
        index = fx.evidence_index(self.sidecars, run)
        check_provenance_claims(self.receipt, index, self.now)
        return index

    def refuses(self, code):
        with self.assertRaises(Refusal) as caught:
            self.check()
        self.assertEqual(code, caught.exception.code)


class SuccessTests(Base):
    def test_coherent_synthetic_claims_are_only_offline_consistent(self):
        index = self.check()
        index.require_all_used()
        self.assertFalse(self.receipt["promotable"])
        # Consistency never produces a runtime capability.
        with self.assertRaises(Refusal) as caught:
            runtime_preflight()
        self.assertEqual("unsupported", caught.exception.code)

    def test_fixture_is_fresh_and_provenance_only(self):
        other, sidecars, _ = fx.provenance_fixture()
        self.assertEqual(self.receipt, other)
        other["scope"]["pr"] = "x"
        self.assertEqual("1639", self.receipt["scope"]["pr"])
        self.assertTrue({r["kind"] for r in sidecars.values()} <= set(PROVENANCE_RECORDS))
        for record in sidecars.values():
            self.assertEqual(self.receipt["attempt"]["run"], record["run"])

    def test_clock_must_be_utc_and_inside_lease(self):
        for now, code in (
            (self.now.replace(tzinfo=None), "clock"),
            (self.now.replace(hour=1), "expiry"),
            (self.now.replace(minute=23), "provenance_time"),
        ):
            self.now = now
            self.refuses(code)

    def test_index_type_is_required(self):
        with self.assertRaises(Refusal) as caught:
            check_provenance_claims(self.receipt, object(), self.now)
        self.assertEqual("evidence_index", caught.exception.code)

    def test_extra_well_formed_sidecar_fails_final_accounting(self):
        extra = copy.deepcopy(self.sidecars["tooling-ref"])
        extra["ref"] = "extra-ref"
        self.sidecars["extra-ref"] = extra
        index = self.check()
        with self.assertRaises(Refusal) as caught:
            index.require_all_used()
        self.assertEqual("unreferenced_evidence", caught.exception.code)


class AttemptTests(Base):
    def test_sidecar_from_another_attempt_refuses_at_intake(self):
        self.sidecars["tooling-ref"]["run"] = dict(OTHER_RUN)
        with self.assertRaises(Refusal) as caught:
            fx.evidence_index(self.sidecars)
        self.assertEqual("evidence_attempt", caught.exception.code)

    def test_index_for_another_attempt_refuses_at_intake(self):
        with self.assertRaises(Refusal) as caught:
            fx.evidence_index(self.sidecars, OTHER_RUN)
        self.assertEqual("evidence_attempt", caught.exception.code)

    def test_receipt_attempt_must_match_index_attempt(self):
        other = copy.deepcopy(self.sidecars)
        for record in other.values():
            record["run"] = dict(OTHER_RUN)
        index = fx.evidence_index(other, OTHER_RUN)
        with self.assertRaises(Refusal) as caught:
            check_provenance_claims(self.receipt, index, self.now)
        self.assertEqual("evidence_attempt", caught.exception.code)

    def test_each_run_bound_claim_must_carry_the_attempt(self):
        paths = {
            "execution": lambda r: r["authority"]["execution"],
            "build": lambda r: r["build"],
            "rollout": lambda r: r["rollout"],
            "lock": lambda r: r["attempt"]["locks"][1],
            "gate": lambda r: r["gates"]["records"][3],
            "coverage": lambda r: r["observations"]["coverage"][0],
            "sample_one": lambda r: r["served"]["samples"][0],
            "served_before": lambda r: r["lifecycle"]["served_before"],
            "served_after": lambda r: r["lifecycle"]["served_after"],
        }
        for name, locate in paths.items():
            with self.subTest(name):
                self.setUp()
                locate(self.receipt)["run"] = dict(OTHER_RUN)
                self.refuses("provenance_attempt")

    def test_control_identity(self):
        cases = {
            "control_is_candidate": lambda r: r["control"]["version"].update(commit=fx.CANDIDATE),
            "workflow_sha": lambda r: r["control"]["workflow"].update(sha=OTHER),
            "run_control_sha": lambda r: r["attempt"]["run"].update(control_sha=OTHER),
        }
        for name, mutate in cases.items():
            with self.subTest(name):
                self.setUp()
                mutate(self.receipt)
                self.refuses("control_identity")

    def test_actor_must_be_the_caller(self):
        self.receipt["attempt"]["actor"]["subject_id"] = fx.SUBJECTS["observer"]
        self.refuses("caller_identity")


class SourceTests(Base):
    def test_source_constants_are_independently_pinned(self):
        cases = {
            "candidate_commit": lambda s: s["candidate"].update(commit=OTHER),
            "candidate_tree": lambda s: s["candidate"].update(tree=OTHER),
            "recorded_commit": lambda s: s["recorded_base"].update(commit=OTHER),
            "recorded_tree": lambda s: s["recorded_base"].update(tree=OTHER),
            "merge_base": lambda s: s["merge_base"].update(commit=OTHER),
        }
        for name, mutate in cases.items():
            with self.subTest(name):
                self.setUp()
                mutate(self.receipt["scope"])
                self.refuses("provenance_source")

    def test_manifest_revisions(self):
        locate = {
            "source": lambda r: r["scope"]["source_manifest"],
            "components": lambda r: r["build"]["components_manifest"],
            "tooling": lambda r: r["control"]["tooling_manifest"],
            "actions": lambda r: r["control"]["actions_manifest"],
            "dependencies": lambda r: r["control"]["dependencies_manifest"],
            "modules": lambda r: r["control"]["modules_manifest"],
            "configuration": lambda r: r["control"]["configuration_manifest"],
        }
        for name, find in locate.items():
            with self.subTest(name):
                self.setUp()
                find(self.receipt)["revision"] = OTHER
                self.refuses("manifest_revision")

    def test_sidecar_digest_or_revision_must_match_each_pin(self):
        refs = ["contract-ref", "policy-ref", "plan-ref", "identity-map-ref",
                "clearance-ref", "source-manifest-ref", "components-ref", "tooling-ref",
                "actions-ref", "dependencies-ref", "modules-ref", "configuration-ref"]
        for ref in refs:
            with self.subTest(ref):
                self.setUp()
                data(self.sidecars, ref)["sha256"] = "0" * 64
                self.refuses("reference_binding")

    def test_resource_plan_must_be_one_document(self):
        self.receipt["authority"]["resource_plan"] = copy.deepcopy(
            self.receipt["authority"]["policy"])
        self.refuses("resource_plan")

    def test_installation_mapping(self):
        cases = {
            "pr": lambda s: data(s, "mapping-ref").update(pr="1640"),
            "head": lambda s: data(s, "mapping-ref")["head"].update(commit=OTHER),
            "squash": lambda s: data(s, "mapping-ref")["squash"].update(commit=OTHER),
            "control": lambda s: data(s, "mapping-ref")["control"].update(commit=OTHER),
        }
        for name, mutate in cases.items():
            with self.subTest(name):
                self.setUp()
                mutate(self.sidecars)
                self.refuses("installation_mapping")
        self.setUp()
        self.receipt["control"]["installation"]["squash"] = copy.deepcopy(fx.SQUASH)
        data(self.sidecars, "mapping-ref")["squash"] = copy.deepcopy(fx.SQUASH)
        self.refuses("installation_mapping")


class JobTests(Base):
    def test_job_provenance(self):
        cases = {
            "wrong_head": lambda s: data(s, "job-control").update(head=OTHER),
            "failed": lambda s: data(s, "job-control").update(conclusion="failure"),
            "candidate_job_on_control_head": lambda s: data(s, "job-build").update(head=fx.CONTROL_SHA),
            "control_job_labeled_candidate": lambda s: data(s, "job-control").update(source="candidate"),
        }
        for name, mutate in cases.items():
            with self.subTest(name):
                self.setUp()
                mutate(self.sidecars)
                self.refuses("job_provenance")

    def test_job_windows_stay_inside_the_attempt(self):
        for field, value in (("started_at", "2025-12-31T23:59:00Z"),
                             ("ended_at", fx.at("00:24:30"))):
            with self.subTest(field):
                self.setUp()
                data(self.sidecars, "job-control")[field] = value
                self.refuses("provenance_time")

    def test_duplicate_service_job_ids(self):
        data(self.sidecars, "job-observer")["id"] = "77"
        self.refuses("job_identity")

    def test_duplicate_references(self):
        for field in ("job_refs", "step_refs", "check_refs", "producer_refs"):
            with self.subTest(field):
                self.setUp()
                self.receipt["attempt"][field] = self.receipt["attempt"][field] * 2
                self.refuses("reference_duplicate")

    def test_producers(self):
        data(self.sidecars, "producer-ref")["workflow"]["id"] = "999"
        self.refuses("producer_binding")
        self.setUp()
        data(self.sidecars, "job-build")["producer_ref"] = "producer-two"
        self.refuses("producer_binding")
        self.setUp()
        extra = copy.deepcopy(self.sidecars["producer-ref"])
        extra["ref"] = "producer-two"
        self.sidecars["producer-two"] = extra
        self.receipt["attempt"]["producer_refs"].append("producer-two")
        self.refuses("producer_binding")  # an unused producer is not accepted

    def test_steps_and_checks_bind_to_successful_jobs(self):
        cases = {
            "step_failed": lambda s: data(s, "step-ref").update(result="failure"),
            "step_unknown_job": lambda s: data(s, "step-ref").update(job_ref="job-other"),
            "check_failed": lambda s: data(s, "check-ref").update(result="failure"),
            "check_head": lambda s: data(s, "check-ref").update(head=OTHER),
        }
        for name, mutate in cases.items():
            with self.subTest(name):
                self.setUp()
                mutate(self.sidecars)
                self.refuses("job_binding")

    def test_duplicate_step_and_check_ids(self):
        data(self.sidecars, "check-ref")["id"] = "6"
        extra = copy.deepcopy(self.sidecars["check-ref"])
        extra["ref"] = "check-two"
        self.sidecars["check-two"] = extra
        self.receipt["attempt"]["check_refs"].append("check-two")
        self.refuses("job_identity")


class BuildTests(Base):
    def test_build_candidate(self):
        self.receipt["build"]["candidate"]["tree"] = OTHER
        self.refuses("build_candidate")

    def test_build_provenance_sidecar(self):
        cases = {
            "build_ref": lambda d: d.update(build_ref="other-build"),
            "candidate": lambda d: d["candidate"].update(tree=OTHER),
            "image": lambda d: d.update(image_digest="sha256:" + "0" * 64),
            "components": lambda d: d.update(components_sha256="0" * 64),
            "worker": lambda d: d["worker"].update(version_id="12121212-1212-4212-8212-121212121212"),
        }
        for name, mutate in cases.items():
            with self.subTest(name):
                self.setUp()
                mutate(data(self.sidecars, "provenance-ref"))
                self.refuses("build_provenance")

    def test_build_isolation(self):
        cases = {
            "build_ref": lambda d: d.update(build_ref="other-build"),
            "candidate": lambda d: d["candidate"].update(commit=OTHER),
            "unknown": lambda d: d.update(outcome="unknown"),
            "credential_path": lambda d: d.update(credential_paths=["docker_socket"]),
            "control_job": lambda d: d.update(job_ref="job-control"),
            "missing_job": lambda d: d.update(job_ref="job-other"),
        }
        for name, mutate in cases.items():
            with self.subTest(name):
                self.setUp()
                mutate(data(self.sidecars, "isolation-ref"))
                self.refuses("build_isolation")

    def test_worker_module_integrity(self):
        cases = {
            "duplicate_module": (lambda w: w["modules"].__setitem__(
                1, dict(w["modules"][0])), "worker_modules"),
            "entrypoint_absent": (lambda w: w.update(entrypoint="dist/none.mjs"),
                                  "worker_entrypoint"),
            "duplicate_flag": (lambda w: w.update(compatibility_flags=["a", "a"]),
                               "reference_duplicate"),
        }
        for name, (mutate, code) in cases.items():
            with self.subTest(name):
                self.setUp()
                set_worker(self.receipt, self.sidecars, mutate)
                self.refuses(code)

    def test_module_change_breaks_served_binding(self):
        set_worker(self.receipt, self.sidecars,
                   lambda w: w["modules"][1].update(path="dist/changed.mjs"))
        self.refuses("candidate_served_binding")


class RolloutTests(Base):
    def test_rollout_binding(self):
        cases = {
            "build_ref": lambda r: r.update(build_ref="other-build"),
            "image": lambda r: r.update(image_digest="sha256:" + "0" * 64),
            "worker_version": lambda r: r.update(
                worker_version_id="12121212-1212-4212-8212-121212121212"),
        }
        for name, mutate in cases.items():
            with self.subTest(name):
                self.setUp()
                mutate(self.receipt["rollout"])
                self.refuses("rollout_binding")

    def test_rollout_window(self):
        self.receipt["rollout"]["started_at"] = "2025-12-31T23:59:00Z"
        self.refuses("provenance_time")
        self.setUp()
        self.receipt["rollout"]["ended_at"] = fx.at("00:24:30")
        self.refuses("provenance_time")

    def test_build_must_finish_before_rollout(self):
        data(self.sidecars, "job-build")["ended_at"] = fx.at("00:03:30")
        self.refuses("build_order")


class ServedTests(Base):
    def test_served_identity(self):
        set_sample(self.receipt, self.sidecars, "served-one", reported_revision=OTHER)
        self.refuses("served_identity")
        self.setUp()
        set_sample(self.receipt, self.sidecars, "served-two", observed_at=fx.at("00:24:30"))
        self.refuses("served_identity")
        self.setUp()
        self.receipt["served"]["samples"][1]["ref"] = "served-one"
        self.refuses("served_identity")

    def test_external_sidecar_must_equal_sample(self):
        for ref in SAMPLE_REFS:
            with self.subTest(ref):
                self.setUp()
                sample = receipt_sample(self.receipt, ref)
                data(self.sidecars, sample["external_source_ref"])["sample"][
                    "target_version"] = "6"
                self.refuses("external_served_binding")

    def test_observer_binding_and_independence(self):
        data(self.sidecars, "observer-ref")["job_ref"] = "job-control"
        self.refuses("observer_binding")
        self.setUp()
        data(self.sidecars, "job-observer")["ended_at"] = fx.at("00:04:00")
        self.refuses("observer_binding")
        self.setUp()
        data(self.sidecars, "observer-ref")["job_ref"] = "job-other"
        self.refuses("observer_binding")

    def alias_observer(self, subject_id):
        """Give observer and its job producer one principal with this subject."""
        for ref in ("observer-ref", "producer-observer"):
            data(self.sidecars, ref)["principal"]["subject_id"] = subject_id

    def test_observer_must_be_its_job_producer(self):
        cases = {
            "subject": lambda p: p.update(subject_id=fx.SUBJECTS["executor"]),
            "ref": lambda p: p.update(ref="other-principal"),
            "root": lambda p: p.update(identity_root_ref="other-root"),
        }
        for name, mutate in cases.items():
            with self.subTest(name):
                self.setUp()
                mutate(data(self.sidecars, "observer-ref")["principal"])
                self.refuses("observer_binding")
        self.setUp()
        data(self.sidecars, "job-observer")["producer_ref"] = "producer-ref"
        self.refuses("producer_binding")  # leaves the observer producer unused

    def test_observer_subject_cannot_alias_any_actor_or_candidate_producer(self):
        aliases = {
            "author": self.receipt["authority"]["author"]["subject_id"],
            "executor": self.receipt["authority"]["executor"]["subject_id"],
            "caller": self.receipt["authority"]["caller"]["subject_id"],
            "candidate_producer": fx.SUBJECTS["producer"],
        }
        for name, subject in aliases.items():
            with self.subTest(name):
                self.setUp()
                self.alias_observer(subject)
                self.refuses("observer_independence")

    def test_alias_check_is_by_subject_not_by_reference_label(self):
        # Different principal and root labels do not hide the same subject.
        self.alias_observer(fx.SUBJECTS["producer"])
        for ref in ("observer-ref", "producer-observer"):
            data(self.sidecars, ref)["principal"].update(
                ref="fresh-label", identity_root_ref="fresh-root")
        self.refuses("observer_independence")

    def test_candidate_samples_bind_independently(self):
        changes = {
            "revision": dict(external_revision=OTHER, reported_revision=OTHER),
            "image": dict(image_digest="sha256:" + "0" * 64),
            "worker": dict(worker_version_id="12121212-1212-4212-8212-121212121212"),
            "modules": dict(modules_sha256="0" * 64),
            "application": dict(application_id="12121212-1212-4212-8212-121212121212"),
            "target": dict(target_version="6"),
            "build": dict(build_ref="other-build"),
            "rollout": dict(rollout_ref="other-rollout"),
            "before_rollout_end": dict(observed_at=fx.at("00:03:30")),
        }
        for name, change in changes.items():
            for ref in ("served-one", "served-two"):
                with self.subTest(name=name, ref=ref):
                    self.setUp()
                    set_sample(self.receipt, self.sidecars, ref, **change)
                    self.refuses("candidate_served_binding")


class RestorationTests(Base):
    def test_before_and_after_identity(self):
        changes = {
            "image": dict(image_digest="sha256:" + "0" * 64),
            "revision": dict(external_revision=OTHER, reported_revision=OTHER),
            "application": dict(application_id="12121212-1212-4212-8212-121212121212"),
            "target": dict(target_version="6"),
            "modules": dict(modules_sha256="0" * 64),
        }
        for name, change in changes.items():
            with self.subTest(name):
                self.setUp()
                set_sample(self.receipt, self.sidecars, "served-after", **change)
                self.refuses("restoration_identity")

    def test_ref_and_observation_ids_may_differ_but_times_must_bracket(self):
        set_sample(self.receipt, self.sidecars, "served-before", observed_at=fx.at("00:03:30"))
        self.refuses("restoration_identity")
        self.setUp()
        set_sample(self.receipt, self.sidecars, "served-after", observed_at=fx.at("00:13:00"))
        self.refuses("restoration_identity")

    def test_restored_worker(self):
        self.receipt["lifecycle"]["worker_after"]["compatibility_date"] = "2026-01-02"
        self.refuses("worker_version_identity")
        self.setUp()
        for ref in ("served-before", "served-after"):
            set_sample(self.receipt, self.sidecars, ref,
                       worker_version_id="12121212-1212-4212-8212-121212121212")
        self.refuses("restoration_worker")
        self.setUp()
        for ref in ("served-before", "served-after"):
            set_sample(self.receipt, self.sidecars, ref, modules_sha256="0" * 64)
        self.refuses("restoration_worker")

    def test_restored_worker_modules_must_be_well_formed(self):
        for key in ("worker_before", "worker_after"):
            self.receipt["lifecycle"][key]["entrypoint"] = "dist/none.mjs"
        self.refuses("worker_entrypoint")


class TimeTests(Base):
    def test_attempt_and_lease_windows(self):
        cases = {
            "starts_before_lease": (lambda r: r["attempt"].update(started_at="2025-12-31T23:59:00Z"),
                                    "provenance_time"),
            "ends_after_now": (lambda r: r["attempt"].update(ended_at=fx.at("00:26:00")),
                               "provenance_time"),
            "lease_too_long": (lambda r: r["authority"]["execution"].update(
                expires_at=fx.at("01:30:00")), "lease"),
            "lease_not_started": (lambda r: r["authority"]["execution"].update(
                not_before=fx.at("00:26:00")), "expiry"),
        }
        for name, (mutate, code) in cases.items():
            with self.subTest(name):
                self.setUp()
                mutate(self.receipt)
                self.refuses(code)

    def test_observation_window(self):
        self.receipt["attempt"]["observation_ended_at"] = fx.at("00:30:00")
        self.refuses("provenance_time")
        self.setUp()
        self.receipt["attempt"]["observation_started_at"] = fx.at("00:01:00")
        self.receipt["attempt"]["observation_ended_at"] = fx.at("00:22:00")
        self.refuses("observation_window")

    def test_case_must_stay_inside_observation_window(self):
        self.receipt["observations"]["cases"][0]["started_at"] = fx.at("00:04:50")
        self.refuses("observation_window")
        self.setUp()
        self.receipt["observations"]["cases"][0]["ended_at"] = fx.at("00:16:00")
        self.refuses("served_bracketing")


class BracketingTests(Base):
    def test_case_served_references(self):
        case = self.receipt["observations"]["cases"][0]
        case["served_ref"] = "served-two"  # observed after the case starts
        self.refuses("served_bracketing")
        self.setUp()
        self.receipt["observations"]["cases"][0]["served_ref"] = "served-before"
        self.refuses("sample_candidate")
        self.setUp()
        self.receipt["observations"]["cases"][0]["served_ref"] = "served-missing"
        self.refuses("unresolved_served_reference")

    def test_no_later_candidate_sample_means_unbracketed(self):
        self.receipt["observations"]["cases"][0]["ended_at"] = fx.at("00:14:30")
        self.refuses("served_bracketing")

    def test_effects_are_bracketed(self):
        for key in ("expected_effects", "actual_effects"):
            with self.subTest(key):
                self.setUp()
                self.receipt["observations"][key][0]["observed_at"] = fx.at("00:15:00")
                self.refuses("served_bracketing")
                self.setUp()
                self.receipt["observations"][key][0]["observed_at"] = fx.at("00:25:00")
                self.refuses("provenance_time")
                self.setUp()
                self.receipt["observations"][key][0]["served_ref"] = "served-missing"
                self.refuses("unresolved_served_reference")

    def test_gate_observation_must_be_bracketed(self):
        set_gate_time(self.receipt, self.sidecars, "user_flow", fx.at("00:15:00"))
        self.refuses("served_bracketing")


def set_gate_served(receipt, sidecars, name, served_ref, when):
    gate(receipt, name)["served_ref"] = served_ref
    data(sidecars, "gs-" + name)["served_ref"] = served_ref
    set_gate_time(receipt, sidecars, name, when)


def use_case(receipt, phase, served_ref, started, ended):
    case = receipt["observations"]["cases"][0]
    case.update(phase=phase, served_ref=served_ref, started_at=fx.at(started),
                ended_at=fx.at(ended))


class RestoredSampleUseTests(Base):
    """Actual restored S samples are legitimate only for non-live uses and times."""

    RUNTIME_GATES = ("rollout", "health", "readiness", "voice", "help", "user_flow")

    def test_restored_pair_ignores_differing_reference_labels(self):
        set_sample(self.receipt, self.sidecars, "served-before", build_ref="label-one",
                   rollout_ref="label-two")
        self.check().require_all_used()

    def test_baseline_may_use_before_sample_only_before_rollout(self):
        use_case(self.receipt, "baseline", "served-before", "00:02:10", "00:02:50")
        self.check()
        use_case(self.receipt, "baseline", "served-before", "00:02:10", "00:03:30")
        self.refuses("served_bracketing")

    def test_before_sample_cannot_precede_its_own_observation(self):
        use_case(self.receipt, "baseline", "served-before", "00:01:00", "00:01:30")
        self.refuses("served_bracketing")

    def test_cleanup_and_restoration_may_use_after_sample(self):
        for phase in ("cleanup", "restoration"):
            with self.subTest(phase):
                self.setUp()
                use_case(self.receipt, phase, "served-after", "00:20:30", "00:22:00")
                self.check()

    def test_after_sample_requires_being_observed_first(self):
        use_case(self.receipt, "cleanup", "served-after", "00:19:00", "00:19:30")
        self.refuses("served_bracketing")

    def test_before_sample_is_not_valid_for_cleanup_after_rollout(self):
        use_case(self.receipt, "cleanup", "served-before", "00:20:30", "00:22:00")
        self.refuses("served_bracketing")

    def test_baseline_may_still_use_a_candidate_sample(self):
        use_case(self.receipt, "baseline", "served-one", "00:06:00", "00:10:00")
        self.check()

    def test_live_phases_reject_restored_samples(self):
        for phase in ("off", "on", "replay", "negative", "final_off"):
            with self.subTest(phase=phase, sample="before"):
                self.setUp()
                use_case(self.receipt, phase, "served-before", "00:06:00", "00:10:00")
                self.refuses("sample_candidate")
            with self.subTest(phase=phase, sample="after"):
                self.setUp()
                use_case(self.receipt, phase, "served-after", "00:06:00", "00:10:00")
                # The after sample is not yet observed inside the live window.
                self.refuses("served_bracketing")

    def test_generic_effects_may_use_restored_samples_at_their_times(self):
        for key in ("expected_effects", "actual_effects"):
            effect = self.receipt["observations"][key][0]
            effect.update(served_ref="served-before", observed_at=fx.at("00:02:30"))
        self.check()
        self.setUp()
        for key in ("expected_effects", "actual_effects"):
            effect = self.receipt["observations"][key][0]
            effect.update(served_ref="served-after", observed_at=fx.at("00:21:00"))
        self.check()

    def test_generic_effects_reject_wrongly_timed_restored_samples(self):
        effect = self.receipt["observations"]["actual_effects"][0]
        effect.update(served_ref="served-before", observed_at=fx.at("00:03:30"))
        self.refuses("served_bracketing")
        self.setUp()
        effect = self.receipt["observations"]["expected_effects"][0]
        effect.update(served_ref="served-after", observed_at=fx.at("00:19:00"))
        self.refuses("served_bracketing")

    def test_planning_gates_may_use_before_sample_before_rollout(self):
        for name in ("schema_plan", "roles"):
            with self.subTest(name):
                self.setUp()
                set_gate_served(self.receipt, self.sidecars, name, "served-before",
                                fx.at("00:02:30"))
                self.check().require_all_used()
        self.setUp()
        set_gate_served(self.receipt, self.sidecars, "roles", "served-before",
                        fx.at("00:03:30"))
        self.refuses("gate_order")

    def test_planning_gates_cannot_be_substituted_by_post_restoration_checks(self):
        set_gate_served(self.receipt, self.sidecars, "schema_plan", "served-after",
                        fx.at("00:21:00"))
        self.refuses("gate_order")

    def test_runtime_gates_reject_restored_samples(self):
        for name in self.RUNTIME_GATES:
            with self.subTest(name=name, sample="before"):
                self.setUp()
                set_gate_served(self.receipt, self.sidecars, name, "served-before",
                                fx.at("00:02:30"))
                self.refuses("sample_candidate")
            with self.subTest(name=name, sample="after"):
                self.setUp()
                set_gate_served(self.receipt, self.sidecars, name, "served-after",
                                fx.at("00:21:00"))
                self.refuses("sample_candidate")


class GateTests(Base):
    def test_gate_job(self):
        cases = {
            "unknown_job": lambda g: g.update(job_id="999"),
        }
        for name, mutate in cases.items():
            with self.subTest(name):
                self.setUp()
                mutate(gate(self.receipt, "roles"))
                self.refuses("gate_job")
        self.setUp()
        data(self.sidecars, "job-control")["source"] = "independent_verifier"
        self.refuses("gate_job")
        self.setUp()
        data(self.sidecars, "job-control")["started_at"] = fx.at("00:14:00")
        self.refuses("gate_job")

    def test_gate_result(self):
        for change in (dict(result="failure"), dict(status=500), dict(status=199)):
            with self.subTest(change):
                self.setUp()
                gate(self.receipt, "health").update(change)
                self.refuses("gate_result")

    def test_duplicate_gate_references(self):
        for field in ("sample_refs", "assertion_refs"):
            with self.subTest(field):
                self.setUp()
                gate(self.receipt, "voice")[field] *= 2
                self.refuses("reference_duplicate")

    def test_gate_sample_fields(self):
        changes = {
            "name": dict(gate_name="help"),
            "job": dict(job_id="88"),
            "served": dict(served_ref="served-two"),
            "time": dict(observed_at=fx.at("00:12:00")),
            "status": dict(status=204),
            "unknown": dict(result="unknown"),
        }
        for name, change in changes.items():
            with self.subTest(name):
                self.setUp()
                data(self.sidecars, "gs-roles").update(change)
                self.refuses("gate_sample")

    def test_gate_assertions(self):
        cases = {
            "failed": lambda d: d.update(outcome="failure"),
            "wrong_gate": lambda d: d.update(gate_name="help"),
            "foreign_sample": lambda d: d.update(sample_refs=["gs-help"]),
            "superset": lambda d: d.update(sample_refs=["gs-roles", "gs-help"]),
        }
        for name, mutate in cases.items():
            with self.subTest(name):
                self.setUp()
                mutate(data(self.sidecars, "ga-roles"))
                self.refuses("gate_assertion")

    def test_every_gate_sample_needs_an_assertion(self):
        extra = copy.deepcopy(self.sidecars["gs-roles"])
        extra["ref"] = "gs-roles-extra"
        self.sidecars["gs-roles-extra"] = extra
        gate(self.receipt, "roles")["sample_refs"].append("gs-roles-extra")
        self.refuses("gate_assertion")

    def test_duplicate_assertion_sample_refs(self):
        data(self.sidecars, "ga-roles")["sample_refs"] = ["gs-roles", "gs-roles"]
        self.refuses("reference_duplicate")


class ReferenceTypeTests(Base):
    def test_every_sidecar_is_required(self):
        for ref in list(self.sidecars):
            with self.subTest(ref):
                self.setUp()
                del self.sidecars[ref]
                self.refuses("unresolved_reference")

    def test_wrong_typed_references(self):
        setters = {
            "mapping": (lambda r, v: r["control"]["installation"].update(mapping_ref=v),
                        "job-build"),
            "provenance": (lambda r, v: r["build"].update(provenance_ref=v), "isolation-ref"),
            "isolation": (lambda r, v: r["build"].update(isolation_receipt_ref=v),
                          "provenance-ref"),
            "job": (lambda r, v: r["attempt"].update(job_refs=[v, "job-control", "job-observer"]),
                    "step-ref"),
            "step": (lambda r, v: r["attempt"].update(step_refs=[v]), "check-ref"),
            "check": (lambda r, v: r["attempt"].update(check_refs=[v]), "step-ref"),
            "producer": (lambda r, v: r["attempt"].update(producer_refs=[v]), "job-build"),
            "external": (lambda r, v: r["served"]["samples"][0].update(external_source_ref=v),
                         "observer-ref"),
            "gate_sample": (lambda r, v: gate(r, "roles").update(sample_refs=[v]),
                            "ga-roles"),
            "gate_assertion": (lambda r, v: gate(r, "roles").update(assertion_refs=[v]),
                               "gs-roles"),
            "scope_manifest": (lambda r, v: r["scope"]["source_manifest"].update(ref=v),
                               "contract-ref"),
            "contract_document": (lambda r, v: r["scope"]["contract"].update(ref=v),
                                  "source-manifest-ref"),
        }
        for name, (setter, wrong) in setters.items():
            with self.subTest(name):
                self.setUp()
                setter(self.receipt, wrong)
                with self.assertRaises(Refusal) as caught:
                    self.check()
                self.assertIn(caught.exception.code,
                              ("reference_kind", "reference_binding"))

    def test_nonexistent_references_are_unresolved(self):
        self.receipt["build"]["provenance_ref"] = "nowhere"
        self.refuses("unresolved_reference")

    def test_sidecar_kind_filename_and_schema_are_closed(self):
        record = self.sidecars["tooling-ref"]
        for mutate in (lambda: record.update(kind="authority_root"),
                       lambda: record.update(schema="other"),
                       lambda: record.update(extra=1),
                       lambda: record["data"].update(extra=1)):
            with self.subTest():
                self.setUp()
                record = self.sidecars["tooling-ref"]
                mutate()
                with self.assertRaises(Refusal):
                    fx.evidence_index(self.sidecars)
        self.setUp()
        files = fx.evidence_files(self.sidecars)
        files["evidence/renamed.json"] = files.pop("evidence/tooling-ref.json")
        with self.assertRaises(Refusal) as caught:
            EvidenceIndex(files, fx.run())
        self.assertEqual("evidence_path", caught.exception.code)


class CrossObjectRegressions(Base):
    def test_restored_source_is_not_inferred_from_main_or_recorded_base(self):
        for ref in ("served-before", "served-after"):
            set_sample(self.receipt, self.sidecars, ref,
                       external_revision=OTHER, reported_revision=OTHER)
        self.check().require_all_used()

    def test_identical_worker_metadata_can_reuse_an_immutable_version(self):
        worker = self.receipt["build"]["worker"]
        for field in ("worker_before", "worker_after"):
            self.receipt["lifecycle"][field] = copy.deepcopy(worker)
        for ref in ("served-before", "served-after"):
            set_sample(self.receipt, self.sidecars, ref,
                       worker_version_id=worker["version_id"],
                       modules_sha256=canonical_digest(worker["modules"]))
        self.check().require_all_used()

    def test_restoration_and_candidate_must_be_on_the_same_application(self):
        other = "99999999-9999-4999-8999-999999999999"
        self.receipt["rollout"]["application_id"] = other
        for ref in ("served-one", "served-two"):
            set_sample(self.receipt, self.sidecars, ref, application_id=other)
        self.refuses("restoration_application")

    def test_one_immutable_worker_version_cannot_describe_two_packages(self):
        version = self.receipt["build"]["worker"]["version_id"]
        for field in ("worker_before", "worker_after"):
            self.receipt["lifecycle"][field]["version_id"] = version
        for ref in ("served-before", "served-after"):
            set_sample(self.receipt, self.sidecars, ref, worker_version_id=version)
        self.refuses("worker_version_identity")


class FrozenControlTests(Base):
    def test_merge_base_tree_cannot_be_replaced_under_same_commit(self):
        self.receipt["scope"]["merge_base"]["tree"] = OTHER
        self.refuses("provenance_source")

    def test_candidate_cannot_be_the_reviewed_installation_head(self):
        head = copy.deepcopy(self.receipt["scope"]["candidate"])
        self.receipt["control"]["installation"]["head"] = copy.deepcopy(head)
        data(self.sidecars, "mapping-ref")["head"] = head
        self.refuses("control_installation")

    def test_source_pr_cannot_be_the_installation_pr(self):
        self.receipt["control"]["installation"]["pr"] = "1639"
        data(self.sidecars, "mapping-ref")["pr"] = "1639"
        self.refuses("control_installation")

    def test_schema_and_role_checks_must_precede_rollout(self):
        for name in ("schema_plan", "roles"):
            with self.subTest(name=name):
                self.setUp()
                set_gate_served(self.receipt, self.sidecars, name, "served-one",
                                fx.at("00:13:00"))
                self.refuses("gate_order")


if __name__ == "__main__":
    unittest.main()
