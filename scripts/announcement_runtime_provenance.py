"""Offline cross-object provenance predicates, never runtime qualification.

Typed claims must agree, but agreeing fabrications still cannot authenticate
installation, isolation, service producers, observers or a verifier root. No
network, build, rollout, credential, cleanup or promotion operation is available.
"""

from datetime import datetime, timedelta, timezone

from announcement_runtime_evidence import EvidenceIndex, canonical_digest
from announcement_runtime_policy import check_lease
from announcement_runtime_schema import require, utc, validate_receipt_shape
from announcement_runtime_source import (
    CANDIDATE, CANDIDATE_TREE, MERGE_BASE, MERGE_BASE_TREE, RECORDED_BASE, RECORDED_BASE_TREE,
)


def _index(records, key, code):
    result = {record[key]: record for record in records}
    require(len(result) == len(records), code)
    return result


def _unique_refs(refs):
    require(len(refs) == len(set(refs)), "reference_duplicate")


def _window(started, ended, lower, upper):
    start, end = utc(started), utc(ended)
    require(lower <= start <= end <= upper, "provenance_time")
    return start, end


def _worker(worker):
    _index(worker["modules"], "path", "worker_modules")
    require(worker["entrypoint"] in {x["path"] for x in worker["modules"]},
            "worker_entrypoint")
    _unique_refs(worker["compatibility_flags"])
    return canonical_digest(worker["modules"])


def _restored_identity(sample):
    # Reference/observation IDs may differ; the actual restored pair must not.
    return {key: value for key, value in sample.items() if key not in (
        "ref", "run", "external_source_ref", "observed_at", "build_ref", "rollout_ref",
    )}


def check_provenance_claims(receipt, evidence, now):
    """Reject inconsistent same-attempt source/build/rollout/served/gate claims.

    Only provenance sidecars are resolved here. This is deliberately NOT a
    complete receipt verifier: authority, resource/effect, lifecycle and final
    authoritative service rereads remain separate mandatory checks. In
    particular, an observer's provisioning_ref is not an authenticated root.
    """
    validate_receipt_shape(receipt)
    require(type(evidence) is EvidenceIndex, "evidence_index")
    require(type(now) is datetime and now.tzinfo is timezone.utc, "clock")
    scope, control = receipt["scope"], receipt["control"]
    attempt, build, rollout = receipt["attempt"], receipt["build"], receipt["rollout"]
    authority, lifecycle = receipt["authority"], receipt["lifecycle"]
    candidate, trusted = scope["candidate"], control["version"]
    require(candidate == {"commit": CANDIDATE, "tree": CANDIDATE_TREE} and
            scope["recorded_base"] == {"commit": RECORDED_BASE, "tree": RECORDED_BASE_TREE}
            and scope["merge_base"] == {"commit": MERGE_BASE, "tree": MERGE_BASE_TREE},
            "provenance_source")
    require(trusted["commit"] != candidate["commit"] and
            control["workflow"]["sha"] == trusted["commit"], "control_identity")
    run = attempt["run"]
    require(run["control_sha"] == trusted["commit"], "control_identity")
    evidence.require_run(run)
    require(attempt["actor"] == authority["caller"], "caller_identity")
    lease_start, lease_end = check_lease(authority["execution"]["not_before"],
                                       authority["execution"]["expires_at"], now)
    start, end = _window(attempt["started_at"], attempt["ended_at"], lease_start, now)
    require(end < lease_end, "provenance_time")
    observation_start, observation_end = _window(attempt["observation_started_at"],
        attempt["observation_ended_at"], start, end)
    require(observation_end - observation_start <= timedelta(minutes=20),
            "observation_window")

    for claim, revision in ((scope["source_manifest"], candidate["commit"]),
                            (build["components_manifest"], candidate["commit"])):
        require(claim["revision"] == revision, "manifest_revision")
        evidence.bind(claim, "manifest")
    for field in ("tooling_manifest", "actions_manifest", "dependencies_manifest",
                  "modules_manifest", "configuration_manifest"):
        require(control[field]["revision"] == trusted["commit"], "manifest_revision")
        evidence.bind(control[field], "manifest")
    for claim in (scope["contract"], authority["policy"], authority["resource_plan"],
                  receipt["resources"]["identity_map"], receipt["resources"]["plan"],
                  receipt["gates"]["readiness_clearance"]):
        evidence.bind(claim, "document")
    require(authority["resource_plan"] == receipt["resources"]["plan"], "resource_plan")
    mapping = evidence.resolve(control["installation"]["mapping_ref"], "installation_mapping")
    require(mapping == {"pr": control["installation"]["pr"],
                        "head": control["installation"]["head"],
                        "squash": control["installation"]["squash"],
                        "control": trusted}, "installation_mapping")
    require(control["installation"]["squash"] == trusted, "installation_mapping")
    require(control["installation"]["pr"] != scope["pr"] and
            control["installation"]["head"]["commit"] != candidate["commit"],
            "control_installation")

    run_claims = [authority["execution"], build, rollout, *attempt["locks"],
                  *receipt["gates"]["records"], *receipt["observations"]["coverage"],
                  *receipt["served"]["samples"], lifecycle["served_before"],
                  lifecycle["served_after"]]
    require(all(claim["run"] == run for claim in run_claims), "provenance_attempt")
    for field in ("job_refs", "step_refs", "check_refs", "producer_refs"):
        _unique_refs(attempt[field])
    jobs = {ref: evidence.resolve(ref, "job") for ref in attempt["job_refs"]}
    by_job_id = _index(list(jobs.values()), "id", "job_identity")
    producers = {ref: evidence.resolve(ref, "producer") for ref in attempt["producer_refs"]}
    used_producers = set()
    for job in jobs.values():
        producer = producers.get(job["producer_ref"])
        require(producer is not None, "producer_binding")
        used_producers.add(job["producer_ref"])
        require(producer["workflow"] == control["workflow"], "producer_binding")
        expected_head = candidate["commit"] if job["source"] == "candidate" else trusted["commit"]
        require(job["head"] == expected_head and job["conclusion"] == "success",
                "job_provenance")
        _window(job["started_at"], job["ended_at"], start, end)
    require(used_producers == producers.keys(), "producer_binding")
    for kind, field in (("step", "step_refs"), ("check", "check_refs")):
        records = [evidence.resolve(ref, kind) for ref in attempt[field]]
        _index(records, "id", "job_identity")
        for record in records:
            job = jobs.get(record["job_ref"])
            require(job is not None and record["result"] == "success", "job_binding")
            if kind == "check":
                require(record["head"] == job["head"], "job_binding")

    require(build["candidate"] == candidate, "build_candidate")
    provenance = evidence.resolve(build["provenance_ref"], "build_provenance")
    require(provenance == {"build_ref": build["ref"], "candidate": candidate,
        "image_digest": build["image_digest"],
        "components_sha256": build["components_manifest"]["sha256"],
        "worker": build["worker"]}, "build_provenance")
    isolation = evidence.resolve(build["isolation_receipt_ref"], "build_isolation")
    build_job = jobs.get(isolation["job_ref"])
    require(isolation["build_ref"] == build["ref"] and isolation["candidate"] == candidate
            and isolation["outcome"] == "isolated" and not isolation["credential_paths"]
            and build_job is not None and build_job["source"] == "candidate", "build_isolation")
    module_digest = _worker(build["worker"])
    require(rollout["build_ref"] == build["ref"] and
            rollout["image_digest"] == build["image_digest"] and
            rollout["worker_version_id"] == build["worker"]["version_id"], "rollout_binding")
    rollout_start, rollout_end = _window(rollout["started_at"], rollout["ended_at"], start, end)
    require(utc(build_job["ended_at"]) <= rollout_start, "build_order")

    samples = receipt["served"]["samples"]
    before, after = lifecycle["served_before"], lifecycle["served_after"]
    by_ref = _index([*samples, before, after], "ref", "served_identity")
    for sample in by_ref.values():
        require(start <= utc(sample["observed_at"]) <= end and
                sample["external_revision"] == sample["reported_revision"], "served_identity")
        external = evidence.resolve(sample["external_source_ref"], "external_served")
        require(external["sample"] == sample, "external_served_binding")
        observer = evidence.resolve(external["observer_ref"], "observer")
        observer_job = jobs.get(observer["job_ref"])
        require(observer_job is not None and observer_job["source"] == "external_observer",
                "observer_binding")
        require(utc(observer_job["started_at"]) <= utc(sample["observed_at"]) <=
                utc(observer_job["ended_at"]), "observer_binding")
        observer_producer = producers[observer_job["producer_ref"]]
        require(observer["principal"] == observer_producer["principal"], "observer_binding")
        excluded = {authority[field]["subject_id"] for field in ("author", "executor", "caller")}
        excluded.update(producers[job["producer_ref"]]["principal"]["subject_id"]
                        for job in jobs.values() if job["source"] == "candidate")
        require(observer["principal"]["subject_id"] not in excluded, "observer_independence")
    for sample in samples:
        require(rollout_end <= utc(sample["observed_at"]) and
                sample["external_revision"] == candidate["commit"] and
                sample["image_digest"] == build["image_digest"] and
                sample["worker_version_id"] == build["worker"]["version_id"] and
                sample["modules_sha256"] == module_digest and
                sample["application_id"] == rollout["application_id"] and
                sample["target_version"] == rollout["target_version"] and
                sample["build_ref"] == build["ref"] and sample["rollout_ref"] == rollout["ref"],
                "candidate_served_binding")
    require(utc(before["observed_at"]) <= rollout_start and
            max(utc(sample["observed_at"]) for sample in samples) <= utc(after["observed_at"])
            and _restored_identity(before) == _restored_identity(after), "restoration_identity")
    require(before["application_id"] == after["application_id"] == rollout["application_id"],
            "restoration_application")
    worker_versions = {}
    for worker in (build["worker"], lifecycle["worker_before"], lifecycle["worker_after"]):
        previous = worker_versions.setdefault(worker["version_id"], worker)
        require(previous == worker, "worker_version_identity")
    require(lifecycle["worker_before"] == lifecycle["worker_after"] and
            before["worker_version_id"] == lifecycle["worker_before"]["version_id"] and
            before["modules_sha256"] == _worker(lifecycle["worker_before"]),
            "restoration_worker")

    def observed_sample(ref, lower, upper, candidate_only=False):
        sample = by_ref.get(ref)
        require(sample is not None, "unresolved_served_reference")
        require(utc(sample["observed_at"]) <= lower, "served_bracketing")
        if sample in samples:
            require(any(utc(other["observed_at"]) >= upper for other in samples),
                    "served_bracketing")
        else:
            require(not candidate_only, "sample_candidate")
            if sample is before:
                require(upper <= rollout_start, "served_bracketing")
            else:
                require(sample is after, "served_bracketing")

    for case in receipt["observations"]["cases"]:
        lower, upper = _window(case["started_at"], case["ended_at"], start, end)
        candidate_only = case["phase"] in ("off", "on", "replay", "negative", "final_off")
        observed_sample(case["served_ref"], lower, upper, candidate_only)
        if candidate_only:
            require(observation_start <= lower <= upper <= observation_end,
                    "observation_window")
    for effect in (*receipt["observations"]["expected_effects"],
                   *receipt["observations"]["actual_effects"]):
        observed = utc(effect["observed_at"])
        require(start <= observed <= end, "provenance_time")
        observed_sample(effect["served_ref"], observed, observed)
    for gate in receipt["gates"]["records"]:
        observed = utc(gate["observed_at"])
        require(start <= observed <= end, "provenance_time")
        if gate["name"] in ("schema_plan", "roles"):
            require(observed <= rollout_start, "gate_order")
        observed_sample(gate["served_ref"], observed, observed,
                        candidate_only=gate["name"] not in ("schema_plan", "roles"))
        job = by_job_id.get(gate["job_id"])
        require(job is not None and job["source"] == "control" and
                utc(job["started_at"]) <= observed <= utc(job["ended_at"]), "gate_job")
        require(gate["result"] == "success" and 200 <= gate["status"] < 300, "gate_result")
        _unique_refs(gate["sample_refs"])
        _unique_refs(gate["assertion_refs"])
        for ref in gate["sample_refs"]:
            sample = evidence.resolve(ref, "gate_sample")
            require(sample["gate_name"] == gate["name"] and
                    sample["job_id"] == gate["job_id"] and
                    sample["served_ref"] == gate["served_ref"] and
                    sample["observed_at"] == gate["observed_at"] and
                    sample["status"] == gate["status"] and sample["result"] == "observed",
                    "gate_sample")
        asserted = set()
        for ref in gate["assertion_refs"]:
            assertion = evidence.resolve(ref, "gate_assertion")
            _unique_refs(assertion["sample_refs"])
            require(assertion["gate_name"] == gate["name"] and
                    assertion["outcome"] == "success" and
                    set(assertion["sample_refs"]) <= set(gate["sample_refs"]), "gate_assertion")
            asserted.update(assertion["sample_refs"])
        require(asserted == set(gate["sample_refs"]), "gate_assertion")
