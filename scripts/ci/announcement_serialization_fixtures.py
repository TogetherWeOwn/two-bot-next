"""SYNTHETIC declared serialization graph; no distributed locks exist here.

All jobs, lock coverage and all-writer witnesses are fabricated. This fixture
cannot establish an estate inventory, actual lock acquisition, effects, stored
cases, lifecycle phase completion or successful cleanup/restoration.
"""

import copy

from announcement_authority_fixtures import principal
from announcement_provenance_fixtures import at, envelope
from announcement_resource_fixtures import repin_authority, repin_resources, resource_fixture
from announcement_runtime_evidence import canonical_digest


def repin_serialization(receipt, records):
    plan = records["runtime-serialization-plan"]["data"]
    records["runtime-resource-plan"]["data"]["serialization_plan_sha256"] = canonical_digest(plan)
    repin_resources(receipt, records)
    repin_authority(receipt, records)


def serialization_fixture():
    receipt, records, now = resource_fixture()
    attempt = receipt["attempt"]

    def add(ref, kind, data):
        records[ref] = envelope(ref, kind, copy.deepcopy(data))

    add("producer-controller", "producer", {"principal": principal("controller"),
        "workflow": receipt["control"]["workflow"], "repository": "TogetherWeOwn/two-bot-next"})
    attempt["producer_refs"].append("producer-controller")
    root_job = records["job-control"]["data"]
    root_job.update(producer_ref="producer-controller", started_at=at("00:00:00"),
                    ended_at=at("00:24:00"))
    add("job-lifecycle", "job", {**root_job, "id": "90"})
    attempt["job_refs"].append("job-lifecycle")
    nodes = [
        {"ref": "controller", "job_ref": "job-control", "parent_refs": [],
         "role": "orchestrator", "acquires": ["deploy-staging"], "witness_ref": "witness-controller"},
        {"ref": "leaf", "job_ref": "job-lifecycle", "parent_refs": ["controller"],
         "role": "writer", "acquires": ["staging-lifecycle"], "witness_ref": "witness-leaf"},
        {"ref": "build", "job_ref": "job-build", "parent_refs": ["controller"],
         "role": "credential_free_build", "acquires": [], "witness_ref": "witness-build"},
        {"ref": "external-observer", "job_ref": "job-observer", "parent_refs": ["controller"],
         "role": "observer", "acquires": [], "witness_ref": "witness-external-observer"},
        {"ref": "resource-observer", "job_ref": "job-resource-observer", "parent_refs": ["controller"],
         "role": "observer", "acquires": [], "witness_ref": "witness-resource-observer"},
    ]
    for lock in attempt["locks"]:
        lock.update(owner_ref=principal("controller")["ref"], fence="9",
                    acquired_at=at("00:00:00"), released_at=at("00:24:00"))
        add(lock["ref"], "serialization_lock", {"claim": lock,
            "node_ref": "controller" if lock["name"] == "deploy-staging" else "leaf",
            "observer_ref": "resource-observer", "boundary": "all_writers_external"})
    for node in nodes:
        job = records[node["job_ref"]]["data"]
        names = ["deploy-staging", "staging-lifecycle"] if node["ref"] == "leaf" else ["deploy-staging"]
        add(node["witness_ref"], "serialization_participant", {"node_ref": node["ref"],
            "job_ref": node["job_ref"], "observer_ref": "resource-observer",
            "started_at": job["started_at"], "ended_at": job["ended_at"],
            "held_lock_refs": ["lock-" + name for name in names],
            "other_writer_refs": [], "outcome": "exclusive"})
    add("runtime-serialization-plan", "serialization_plan", {
        "inventory_manifest": receipt["resources"]["inventory_manifest"],
        "nodes": nodes, "writer_refs": ["leaf"]})
    repin_serialization(receipt, records)
    return receipt, records, now
