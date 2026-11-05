"""Offline full-window lock graph comparisons, NOT distributed serialization.

No lock is acquired, writer fenced or observer authenticated here. Claims about
all-writer enforcement cannot prove that external writers honor it. The existing
substrate still lacks that capability; runtime_preflight remains unsupported.
This narrow profile models one parent and one lifecycle leaf, with no reacquire.
"""

from announcement_runtime_evidence import canonical_digest
from announcement_runtime_provenance import _index, _unique_refs, check_provenance_claims
from announcement_runtime_resources import PLAN_REF, check_resource_graph_claims
from announcement_runtime_schema import require, utc


SERIALIZATION_REF = "runtime-serialization-plan"


def check_serialization_claims(receipt, evidence, now):
    """Bind the complete declared job/lock graph to the resource-plan scope.

    The parent holds deploy-staging for the entire attempt, including snapshot,
    drain, cleanup and restoration. Writers descend through a distinct leaf
    holding staging-lifecycle; ancestors never acquire a child's identical lock.
    Multiple leaves/reacquisition are unsupported, not silently flattened. This
    checks declared participants only, not the completeness of a real estate's
    writer inventory or actual lifecycle phase, queue or restoration semantics.
    """
    check_provenance_claims(receipt, evidence, now)
    check_resource_graph_claims(receipt, evidence, now)
    resource_plan = evidence.resolve(PLAN_REF, "resource_plan_content")
    plan = evidence.resolve(SERIALIZATION_REF, "serialization_plan")
    require(canonical_digest(plan) == resource_plan["serialization_plan_sha256"],
            "serialization_scope")
    attempt, authority = receipt["attempt"], receipt["authority"]
    start, end = utc(attempt["started_at"]), utc(attempt["ended_at"])
    require(start < end, "serialization_window")
    require(plan["inventory_manifest"] == receipt["resources"]["inventory_manifest"],
            "serialization_inventory")
    evidence.bind(plan["inventory_manifest"], "manifest")
    nodes = _index(plan["nodes"], "ref", "serialization_nodes")
    require(len({n["job_ref"] for n in nodes.values()}) == len(nodes) and
            {n["job_ref"] for n in nodes.values()} == set(attempt["job_refs"]),
            "serialization_jobs")
    _unique_refs(plan["writer_refs"])
    require(set(plan["writer_refs"]) == {ref for ref, n in nodes.items() if n["role"] == "writer"},
            "serialization_writers")
    roots = [n for n in nodes.values() if not n["parent_refs"]]
    require(len(roots) == 1 and roots[0]["role"] == "orchestrator" and
            roots[0]["acquires"] == ["deploy-staging"], "serialization_root")
    root = roots[0]
    require(sum(n["role"] == "orchestrator" for n in nodes.values()) == 1,
            "serialization_root")
    locks = _index(attempt["locks"], "name", "serialization_locks")
    require(set(locks) == {"deploy-staging", "staging-lifecycle"} and
            len({lock["ref"] for lock in locks.values()}) == 2, "serialization_locks")
    source = evidence.resolve(authority["source_ref"], "authority_source")
    controller = evidence.resolve(source["controller_ref"], "authority_consumer")
    verifier = evidence.resolve(source["verifier_ref"], "authority_consumer")
    jobs = {ref: evidence.resolve(ref, "job") for ref in attempt["job_refs"]}
    excluded = {authority[role]["subject_id"] for role in ("author", "executor", "caller")}
    excluded.add(controller["principal"]["subject_id"])
    excluded.update(evidence.resolve(job["producer_ref"], "producer")["principal"]["subject_id"]
                    for job in jobs.values() if job["source"] in ("candidate", "control"))

    def observer(ref):
        witness = evidence.resolve(ref, "observer")
        require(witness["principal"]["subject_id"] not in excluded, "serialization_independence")
        job = jobs.get(witness["job_ref"])
        require(witness["principal"] == verifier["principal"] and
                witness["provisioning_ref"] == verifier["provisioning_ref"] and
                job is not None and job["source"] in ("external_observer", "independent_verifier")
                and utc(job["started_at"]) <= start and end <= utc(job["ended_at"]),
                "serialization_observer")
        producer = evidence.resolve(job["producer_ref"], "producer")
        require(producer["principal"] == witness["principal"], "serialization_observer")

    # Walking parent pointers is bounded by the closed node inventory. Do not
    # recurse: an adversarial cycle must refuse, not hit Python's recursion cap.
    paths = {}
    for ref in nodes:
        path, seen, current = [], set(), ref
        while True:
            require(current in nodes and current not in seen, "serialization_cycle")
            seen.add(current)
            node = nodes[current]
            path.append(node)
            if not node["parent_refs"]:
                break
            current = node["parent_refs"][0]
        path.reverse()
        require(path[0] == root, "serialization_root")
        acquired = [name for node in path for name in node["acquires"]]
        require(len(acquired) == len(set(acquired)), "serialization_nested_lock")
        require(acquired in (["deploy-staging"], ["deploy-staging", "staging-lifecycle"]),
                "serialization_lock_order")
        paths[ref] = path

    acquisitions, witness_refs = {}, []
    for ref, node in nodes.items():
        job = jobs[node["job_ref"]]
        role = node["role"]
        if role in ("orchestrator", "writer"):
            producer = evidence.resolve(job["producer_ref"], "producer")
            require(job["source"] == "control" and producer["principal"] == controller["principal"],
                    "serialization_job_role")
        elif role == "observer":
            require(job["source"] in ("external_observer", "independent_verifier") and
                    not node["acquires"], "serialization_job_role")
        else:
            isolation = evidence.resolve(receipt["build"]["isolation_receipt_ref"], "build_isolation")
            require(job["source"] == "candidate" and isolation["job_ref"] == node["job_ref"]
                    and not node["acquires"], "serialization_job_role")
        path = paths[ref]
        names = [name for ancestor in path for name in ancestor["acquires"]]
        if role == "writer":
            require(names == ["deploy-staging", "staging-lifecycle"], "serialization_writer_fence")
        for name in node["acquires"]:
            require(name not in acquisitions, "serialization_reacquire")
            acquisitions[name] = ref
        witness_ref = node["witness_ref"]
        witness_refs.append(witness_ref)
        witness = evidence.resolve(witness_ref, "serialization_participant")
        require(witness["node_ref"] == ref and witness["job_ref"] == node["job_ref"] and
                witness["started_at"] == job["started_at"] and
                witness["ended_at"] == job["ended_at"] and
                witness["held_lock_refs"] == [locks[name]["ref"] for name in names],
                "serialization_participant")
        require(witness["outcome"] == "exclusive" and not witness["other_writer_refs"],
                "serialization_interference")
        observer(witness["observer_ref"])
        lower, upper = utc(job["started_at"]), utc(job["ended_at"])
        require(lower < upper, "serialization_window")
        for ancestor in path[:-1]:
            parent_job = jobs[ancestor["job_ref"]]
            require(utc(parent_job["started_at"]) <= lower and
                    upper <= utc(parent_job["ended_at"]), "serialization_parent_window")
        for name in names:
            lock = locks[name]
            require(utc(lock["acquired_at"]) <= lower and upper <= utc(lock["released_at"]),
                    "serialization_lock_window")
    _unique_refs(witness_refs)
    require(set(acquisitions) == set(locks), "serialization_locks")
    for name, lock in locks.items():
        record = evidence.resolve(lock["ref"], "serialization_lock")
        require(record["claim"] == lock and record["node_ref"] == acquisitions[name] and
                record["boundary"] == "all_writers_external", "serialization_lock_binding")
        require(lock["run"] == attempt["run"] and
                lock["owner_ref"] == controller["principal"]["ref"] and
                lock["fence"] == attempt["enforcement_epoch"] and not lock["interference_refs"],
                "serialization_fence")
        observer(record["observer_ref"])
        holder = jobs[nodes[acquisitions[name]]["job_ref"]]
        require(lock["acquired_at"] == holder["started_at"] and
                lock["released_at"] == holder["ended_at"] and
                start <= utc(lock["acquired_at"]) < utc(lock["released_at"]) <= end,
                "serialization_lock_window")
    parent = locks["deploy-staging"]
    require(parent["acquired_at"] == attempt["started_at"] and
            parent["released_at"] == attempt["ended_at"], "serialization_full_window")
