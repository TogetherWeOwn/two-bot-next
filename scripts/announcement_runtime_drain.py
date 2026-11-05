"""Offline work-inventory/quiescence claims, NOT an installed drain or fence.

This compares a declared finite inventory and independently attributed snapshots
with the lifecycle ledger and full-window telemetry claim. It cannot establish
inventory completeness, authenticate an observer, cancel work, resolve actual
requests or authorize cleanup. Other claim checkers remain mandatory and runtime
admission remains unconditionally unsupported.
"""

from announcement_runtime_evidence import canonical_digest
from announcement_runtime_lifecycle import check_lifecycle_ledger_claims
from announcement_runtime_provenance import _index, _unique_refs
from announcement_runtime_schema import require, utc


INVENTORY_REF = "runtime-work-inventory"


def check_drain_claims(receipt, evidence, now):
    """Reject nonempty, partial, late or self-observed quiescence claims.

    All records are untrusted. In particular an empty declared surface list is
    not proof that unknown surfaces do not exist, and a coherent fabricated
    inventory/fence/snapshot grants no capability. Restoration and separately
    scoped recovery authority are NOT resolved by this helper.
    """
    check_lifecycle_ledger_claims(receipt, evidence, now)
    attempt, life, resources = (receipt[x] for x in ("attempt", "lifecycle", "resources"))
    start, end = utc(attempt["started_at"]), utc(attempt["ended_at"])
    cutoff = utc(attempt["observation_ended_at"])
    snapshot = utc(life["served_before"]["observed_at"])
    restored = utc(life["served_after"]["observed_at"])
    manifest = resources["inventory_manifest"]
    evidence.bind(manifest, "manifest")
    inventory = evidence.resolve(INVENTORY_REF, "work_inventory_content")
    require(canonical_digest(inventory) == manifest["sha256"] and
            inventory["candidate"] == receipt["scope"]["candidate"] and
            inventory["control"] == receipt["control"]["version"] and
            inventory["source_manifest"] == receipt["scope"]["source_manifest"],
            "drain_inventory_binding")
    surfaces = _index(inventory["surfaces"], "ref", "drain_surface_inventory")
    require(bool(surfaces), "drain_surface_inventory")
    for surface in surfaces.values():
        require(not any(part in (".", "..") for part in surface["path"].split("/")),
                "drain_source_path")
    drain = evidence.resolve(life["drain_ref"], "work_drain")
    require(drain["inventory_ref"] == INVENTORY_REF and drain["outcome"] == "resolved" and
            not drain["unknown_operation_refs"], "drain_unresolved")
    drain_start, drain_end = utc(drain["started_at"]), utc(drain["ended_at"])
    cleanup_start = min(utc(x["observed_at"]) for x in life["cleanup"])
    require(cutoff == drain_start <= drain_end <= cleanup_start <= restored <= end,
            "drain_window")

    authority = receipt["authority"]
    source = evidence.resolve(authority["source_ref"], "authority_source")
    controller = evidence.resolve(source["controller_ref"], "authority_consumer")
    require(controller["role"] == "controller", "drain_owner")
    fence = evidence.resolve(drain["fence_ref"], "work_fence")
    require(fence == {"source_ref": authority["source_ref"],
        "execution_ref": authority["execution"]["ref"], "owner_ref": source["controller_ref"],
        "nonce_ref": authority["execution"]["nonce_ref"],
        "authority_epoch": authority["epoch"], "resource_epoch": attempt["enforcement_epoch"],
        "inventory_sha256": manifest["sha256"], "closed_at": attempt["observation_ended_at"],
        "held_until": attempt["ended_at"], "boundary": "external_all_paths", "outcome": "closed"},
        "drain_fence")

    observations = receipt["observations"]
    require(len(observations["coverage"]) == 1, "drain_coverage")
    coverage = observations["coverage"][0]
    effects = observations["actual_effects"]
    require(coverage["ref"] == drain["coverage_ref"] and
            coverage["run"] == attempt["run"] and coverage["result"] == "complete" and
            coverage["started_at"] == attempt["started_at"] and
            coverage["ended_at"] == attempt["ended_at"] and
            coverage["source_inventory_sha256"] == manifest["sha256"] and
            coverage["enforcement_epoch"] == attempt["enforcement_epoch"] and
            coverage["first_sequence"] == 1 and coverage["last_sequence"] == len(effects),
            "drain_coverage")
    window = evidence.resolve(coverage["ref"], "telemetry_window")
    require(window["claim"] == coverage and
            window["operation_ids"] == [x["operation_id"] for x in effects], "drain_coverage")
    _unique_refs(window["operation_ids"])
    # This is only the post-cutoff DB-cleanup projection. Actual wire receipts,
    # atomic transactions and every operation's authority are checked elsewhere.
    cleanup_ids = {x["operation_id"] for x in life["cleanup"]}
    require(all(utc(x["observed_at"]) < cutoff or
                (x["operation_id"] in cleanup_ids and utc(x["observed_at"]) >= drain_end)
                for x in effects), "drain_late_operation")

    observer = evidence.resolve(coverage["observer_ref"], "observer")
    jobs = {ref: evidence.resolve(ref, "job") for ref in attempt["job_refs"]}
    _unique_refs(attempt["job_refs"])
    job = jobs.get(observer["job_ref"])
    require(job is not None and job["source"] == "external_observer" and
            job["head"] == receipt["control"]["version"]["commit"] and
            job["conclusion"] == "success" and
            utc(job["started_at"]) <= start <= end <= utc(job["ended_at"]) <= now and
            job["producer_ref"] in attempt["producer_refs"], "drain_observer")
    producer = evidence.resolve(job["producer_ref"], "producer")
    require(producer["principal"] == observer["principal"] and
            producer["workflow"] == receipt["control"]["workflow"], "drain_observer")
    excluded = {authority[x]["subject_id"] for x in ("author", "executor", "caller")}
    excluded.add(controller["principal"]["subject_id"])
    excluded.update(evidence.resolve(x["producer_ref"], "producer")["principal"]["subject_id"]
                    for x in jobs.values() if x["source"] != "external_observer")
    excluded.update(evidence.resolve(x["owner"], "resource_owner")["principal"]["subject_id"]
                    for x in resources["entries"])
    require(observer["principal"]["subject_id"] not in excluded, "drain_independence")

    refs = [life["queue_baseline_ref"], drain["drained_snapshot_ref"], life["queue_final_ref"]]
    _unique_refs(refs)
    for ref, lower, upper in ((refs[0], start, snapshot),
                              (refs[1], drain_end, drain_end), (refs[2], end, end)):
        state = evidence.resolve(ref, "work_snapshot")
        require(state["inventory_sha256"] == manifest["sha256"] and
                state["observer_ref"] == coverage["observer_ref"] and
                state["enforcement_epoch"] == attempt["enforcement_epoch"] and
                state["outcome"] == "observed" and
                lower <= utc(state["observed_at"]) <= upper, "drain_snapshot")
        actual = _index(state["surfaces"], "surface_ref", "drain_surface_inventory")
        require(actual.keys() == surfaces.keys(), "drain_surface_inventory")
        require(all(x[key] == 0 for x in actual.values()
                    for key in ("queued", "in_flight", "claimed", "delayed")), "drain_nonempty")
