"""Offline full-window effect/receipt correspondence, NOT trusted observation.

No observer is provisioned, path monitored, authority acquired or effect made
here. Coherent fabricated claims still grant no runtime capability. Exact route,
content, source/parser, stored-case, trigger and lifecycle enforcement remain
mandatory separate checks; this module is not a complete receipt verifier.
"""

from collections import Counter

from announcement_runtime_evidence import canonical_digest
from announcement_runtime_policy import check_effect_reconciliation
from announcement_runtime_provenance import _index, _unique_refs, check_provenance_claims
from announcement_runtime_resources import PLAN_REF, check_resource_graph_claims
from announcement_runtime_schema import require, utc


PLATFORM_METHODS = frozenset(("feed_post", "interaction_ack", "interaction_edit",
                              "guild_commands_replace"))


def check_telemetry_claims(receipt, evidence, now):
    """Require one exact gap-free start-through-restoration coverage claim.

    Multiple/subwindow coverage is unsupported rather than inferred complete.
    This checks same-attempt sidecar correspondence, not independence in fact.
    DB atomic grouping is compared, not inferred from matching aggregate counts.
    """
    check_provenance_claims(receipt, evidence, now)
    check_resource_graph_claims(receipt, evidence, now)
    plan = evidence.resolve(PLAN_REF, "resource_plan_content")
    observations, attempt = receipt["observations"], receipt["attempt"]
    resources = receipt["resources"]
    by_resource = _index(resources["entries"], "ref", "resource_inventory")
    require(len(observations["coverage"]) == 1, "telemetry_window_inventory")
    coverage = observations["coverage"][0]
    start, end = utc(attempt["started_at"]), utc(attempt["ended_at"])
    require(utc(coverage["started_at"]) == start and utc(coverage["ended_at"]) == end,
            "telemetry_full_window")
    require(coverage["enforcement_epoch"] == attempt["enforcement_epoch"] and
            coverage["source_inventory_sha256"] == resources["inventory_manifest"]["sha256"],
            "telemetry_scope")
    evidence.bind(resources["inventory_manifest"], "manifest")
    window = evidence.resolve(coverage["ref"], "telemetry_window")
    effects = observations["actual_effects"]
    require(window["claim"] == coverage and
            window["operation_ids"] == [x["operation_id"] for x in effects],
            "telemetry_operations")
    check_effect_reconciliation(observations["expected_effects"], effects, coverage,
        resources["entries"], started_at=attempt["started_at"],
        ended_at=attempt["ended_at"], now=now)
    by_operation = _index(effects, "operation_id", "operation_replay")

    # Observers must be distinct from writers/callers, bound to a successful
    # external-observer job spanning the ENTIRE window, not just one sample.
    observer = evidence.resolve(coverage["observer_ref"], "observer")
    jobs = {ref: evidence.resolve(ref, "job") for ref in attempt["job_refs"]}
    job = jobs.get(observer["job_ref"])
    require(job is not None and job["source"] == "external_observer" and
            utc(job["started_at"]) <= start <= end <= utc(job["ended_at"]),
            "telemetry_observer_window")
    producer = evidence.resolve(job["producer_ref"], "producer")
    require(observer["principal"] == producer["principal"], "telemetry_observer")
    excluded = {receipt["authority"][field]["subject_id"]
                for field in ("author", "executor", "caller")}
    excluded.update(evidence.resolve(j["producer_ref"], "producer")["principal"]["subject_id"]
                    for j in jobs.values() if j["source"] != "external_observer")
    # Resource ownership binds the controller even when it has no job producer.
    excluded.update(evidence.resolve(resource["owner"], "resource_owner")["principal"]["subject_id"]
                    for resource in by_resource.values())
    require(observer["principal"]["subject_id"] not in excluded, "telemetry_independence")

    def observed(record, timestamp):
        require(record["observer_ref"] == coverage["observer_ref"] and
                start <= utc(timestamp) <= end, "telemetry_observation")

    def binding(effect):
        return {"operation_id": effect["operation_id"],
                "effect_sha256": canonical_digest(effect),
                "resource_identity_sha256": canonical_digest(
                    by_resource[effect["resource_ref"]]["identity"])}

    all_refs = []
    for field in ("wire_receipt_refs", "platform_receipt_refs", "transaction_receipt_refs"):
        _unique_refs(coverage[field])
        all_refs.extend(coverage[field])
    _unique_refs(all_refs)
    wire_ids, platform_ids, database_ids = [], [], []
    accepted_ids, transition_refs, transaction_ids = [], [], []
    for ref in coverage["wire_receipt_refs"]:
        wire = evidence.resolve(ref, "wire_attempt")
        effect = by_operation.get(wire["operation_id"])
        require(effect is not None and not effect["method"].startswith("db_"),
                "wire_operation")
        require(wire == {**binding(effect), "observer_ref": coverage["observer_ref"],
                        "observed_at": effect["observed_at"], "outcome": "accepted"},
                "wire_binding")
        wire_ids.append(effect["operation_id"])
    for ref in coverage["platform_receipt_refs"]:
        platform = evidence.resolve(ref, "platform_effect")
        effect = by_operation.get(platform["operation_id"])
        require(effect is not None and effect["method"] in PLATFORM_METHODS,
                "platform_operation")
        observed(platform, platform["observed_at"])
        require(utc(effect["observed_at"]) <= utc(platform["observed_at"]),
                "platform_order")
        require(platform == {"operation_id": effect["operation_id"],
            "resource_identity_sha256": binding(effect)["resource_identity_sha256"],
            "method": effect["method"], "payload_sha256": effect["payload_sha256"],
            "nonce_refs": effect["nonce_ref"], "platform_ids": effect["platform_id_refs"],
            "observer_ref": coverage["observer_ref"], "observed_at": platform["observed_at"],
            "outcome": "accepted"}, "platform_binding")
        _unique_refs(platform["platform_ids"])
        require(effect["method"] == "guild_commands_replace" or
                len(platform["platform_ids"]) == 1, "platform_count")
        accepted_ids.extend((effect["method"], identifier) for identifier in platform["platform_ids"])
        platform_ids.append(effect["operation_id"])

    positive_ids = []
    for slot in ("rsvp_ref", "rsvp_audit_ref"):
        selected = [x["operation_id"] for x in effects
                    if x["resource_ref"] == plan["slots"][slot] and x["method"] == "db_insert"]
        require(len(selected) == 1, "rsvp_effect_inventory")
        positive_ids.extend(selected)
    rsvp_insert, audit_insert = positive_ids
    for ref in coverage["transaction_receipt_refs"]:
        transaction = evidence.resolve(ref, "db_transaction")
        observed(transaction, transaction["committed_at"])
        require(transaction["outcome"] == "committed", "uncertain_transaction")
        transaction_ids.append(transaction["transaction_id"])
        _unique_refs(transaction["transition_refs"])
        transaction_operations = [x["operation_id"] for x in transaction["effects"]]
        if rsvp_insert in transaction_operations or audit_insert in transaction_operations:
            pair = {rsvp_insert, audit_insert}
            extras = [by_operation.get(op) for op in transaction_operations if op not in pair]
            require(pair <= set(transaction_operations) and
                    all(effect is not None and effect["method"] == "db_read" for effect in extras),
                    "rsvp_atomic_audit")
        expected_transitions = []
        for bound in transaction["effects"]:
            effect = by_operation.get(bound["operation_id"])
            require(effect is not None and effect["method"].startswith("db_"),
                    "transaction_operation")
            resource = by_resource[effect["resource_ref"]]
            require(resource["identity"]["kind"] == "database" and bound == binding(effect),
                    "transaction_binding")
            require(effect["observed_at"] == transaction["committed_at"] and
                    not effect["platform_id_refs"] and not effect["nonce_ref"],
                    "transaction_binding")
            database_ids.append(effect["operation_id"])
            if effect["method"] == "db_read":
                require(not effect["db_transition_refs"], "read_transition")
                continue
            require(len(effect["db_transition_refs"]) == 1, "transition_inventory")
            transition_ref = effect["db_transition_refs"][0]
            transition = evidence.resolve(transition_ref, "db_transition")
            claim = transition["claim"]
            require(transition["transaction_ref"] == ref and
                    transition["method"] == effect["method"] and
                    claim["operation_id"] == effect["operation_id"] and
                    claim["resource_ref"] == effect["resource_ref"] and
                    claim["key_sha256"] == resource["identity"]["key_sha256"] and
                    claim["owner"] == resource["owner"] and
                    claim["intended_after_state_ref"] == claim["actual_after_state_ref"] and
                    claim["expected_count"] == claim["actual_count"] == 1 and
                    claim["observed_at"] == effect["observed_at"] and
                    claim["outcome"] == "matched", "transition_binding")
            expected_transitions.append(transition_ref)
            transition_refs.append(transition_ref)
        require(Counter(transaction["transition_refs"]) == Counter(expected_transitions),
                "transaction_transitions")
    _unique_refs(transaction_ids)
    _unique_refs(transition_refs)
    require(len(accepted_ids) == len(set(accepted_ids)), "platform_replay")
    require(Counter(wire_ids) == Counter(x["operation_id"] for x in effects
                                        if not x["method"].startswith("db_")),
            "wire_inventory")
    require(Counter(platform_ids) == Counter(x["operation_id"] for x in effects
                                            if x["method"] in PLATFORM_METHODS),
            "platform_inventory")
    require(Counter(database_ids) == Counter(x["operation_id"] for x in effects
                                            if x["method"].startswith("db_")),
            "transaction_inventory")
    for effect in effects:
        _unique_refs(effect["nonce_ref"])
        require(not effect["db_transition_refs"] or effect["method"].startswith("db_"),
                "wire_transition")
        if effect["method"] not in PLATFORM_METHODS:
            require(not effect["platform_id_refs"], "unplanned_platform_identity")
        if effect["method"] == "feed_post":
            require(effect["nonce_ref"] == [receipt["fixtures"]["nonce_ref"]], "feed_nonce")
        else:
            require(not effect["nonce_ref"], "unplanned_nonce")
