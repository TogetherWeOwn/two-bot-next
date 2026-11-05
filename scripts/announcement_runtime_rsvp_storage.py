"""Restricted first-RSVP row/audit projections, NOT SQL or atomicity proof.

Binds the source-derived first-response columns to literal keys, one declared
transaction and two post-write state digests. This does not execute SQL, prove
locks/triggers/actual stored values, authenticate observers or qualify later
compensation/cleanup/restoration. Runtime admission remains unsupported/HOLD.
"""

from announcement_runtime_evidence import canonical_digest
from announcement_runtime_inputs import INPUT_PLAN_REF
from announcement_runtime_interactions import check_interaction_exchange_claims
from announcement_runtime_provenance import _index
from announcement_runtime_resources import PLAN_REF
from announcement_runtime_schema import require, utc


def check_rsvp_storage_claims(receipt, evidence, now):
    """Compare one fresh RSVP/audit declaration, not re-response or recovery.

    C's discord/rsvp.rs constructs the response timestamp after the pre-reads;
    core/rsvp.rs derives the audit from that same record and interaction ID.
    The two inserts share one committed transaction in core/rsvp_store.rs.
    Projections use whole-second UTC and an explicit SQL-null reason marker.
    """
    check_interaction_exchange_claims(receipt, evidence, now)
    inputs = evidence.resolve(INPUT_PLAN_REF, "input_plan_content")
    plan = evidence.resolve(inputs["interactions"]["ref"], "interaction_plan")
    content = evidence.resolve(plan["exchanges"]["ref"], "interaction_exchanges")
    storage = content["storage"]
    selected = [entry for entry in plan["interactions"] if entry["phase"] == "on"]
    require(len(selected) == 1 and selected[0]["command"] == "rsvp" and
            storage["case_id"] == selected[0]["case_id"], "rsvp_storage_scope")
    interaction = selected[0]
    cases = _index(receipt["observations"]["cases"], "case_id", "case_inventory")
    case = cases[storage["case_id"]]
    exchange = next(entry for entry in content["exchanges"] if entry["case_id"] == case["case_id"])
    slots = evidence.resolve(PLAN_REF, "resource_plan_content")["slots"]
    keys = {entry["resource_ref"]: evidence.resolve(entry["key"]["ref"], "literal_db_key")
            for entry in inputs["keys"]}
    key, audit_key = keys[slots["rsvp_ref"]], keys[slots["rsvp_audit_ref"]]
    require(key["table"] == "event_rsvps" and audit_key["table"] == "announcements_audit_log",
            "rsvp_storage_key")
    request, row, audit = exchange["request"], storage["rsvp"], storage["audit"]
    require({field: key[field] for field in ("guild_id", "event_id", "user_id")} ==
            {"guild_id": request["guild_id"], "event_id": next(option["value"] for option in
                request["options"] if option["name"] == "event-id"), "user_id": request["actor_id"]},
            "rsvp_storage_key")
    require(row == {"guild_id": key["guild_id"], "event_id": key["event_id"],
        "user_id": key["user_id"], "status": inputs["rsvp"]["status"],
        "responded_at": row["responded_at"]}, "rsvp_storage_row")
    require(audit_key["id"] == "rsvp:" + interaction["interaction_id"] and audit == {
        "id": audit_key["id"], "guild_id": row["guild_id"], "actor_id": row["user_id"],
        "action": "event.rsvp", "target_key": row["event_id"], "outcome": row["status"],
        "reason": {"kind": "sql_null"}, "created_at": row["responded_at"]}, "rsvp_storage_audit")
    effects = _index(receipt["observations"]["actual_effects"], "operation_id", "operation_replay")
    writes = []
    for slot in ("rsvp_ref", "rsvp_audit_ref"):
        selected_writes = [effects[ref] for ref in case["actual_effect_refs"]
                           if effects[ref]["resource_ref"] == slots[slot] and
                           effects[ref]["method"] == "db_insert"]
        require(len(selected_writes) == 1, "rsvp_storage_writes")
        writes.append(selected_writes[0])
    require(writes[0]["sequence"] < writes[1]["sequence"], "rsvp_storage_order")
    coverage = receipt["observations"]["coverage"][0]
    transaction_ref = storage["transaction_ref"]
    require(transaction_ref in coverage["transaction_receipt_refs"], "rsvp_storage_transaction")
    transaction = evidence.resolve(transaction_ref, "db_transaction")
    operations = {entry["operation_id"] for entry in transaction["effects"]}
    require({effect["operation_id"] for effect in writes} <= operations and
            transaction["outcome"] == "committed", "rsvp_storage_transaction")
    responded = utc(row["responded_at"])
    pre_reads = sorted(exchange["eligibility_reads"],
                       key=lambda entry: effects[entry["operation_id"]]["sequence"])[:2]
    require(utc(case["started_at"]) <= responded <= utc(transaction["committed_at"]) and
            all(utc(entry["received_at"]) <= responded for entry in pre_reads), "rsvp_storage_time")
    resources = _index(receipt["resources"]["entries"], "ref", "resource_inventory")
    seen = {resources[slots[slot]][field] for slot in ("rsvp_ref", "rsvp_audit_ref")
            for field in ("before_state_ref", "after_state_ref")}
    for effect, projection in zip(writes, (row, audit)):
        resource = resources[effect["resource_ref"]]
        before = evidence.resolve(resource["before_state_ref"], "resource_state")
        require(before["claim"]["presence"] == "absent" and
                utc(before["observed_at"]) <= responded, "rsvp_storage_baseline")
        transition = evidence.resolve(effect["db_transition_refs"][0], "db_transition")
        claim = transition["claim"]
        after_ref = claim["actual_after_state_ref"]
        require(transition["transaction_ref"] == transaction_ref and
                claim["before_state_ref"] == resource["before_state_ref"] and
                claim["intended_after_state_ref"] == after_ref and after_ref not in seen,
                "rsvp_storage_transition")
        after = evidence.resolve(after_ref, "resource_state")
        require(after["claim"]["ref"] == after_ref and
                after["resource_ref"] == resource["ref"] and
                after["identity_sha256"] == canonical_digest(resource["identity"]) and
                after["owner_ref"] == resource["owner"] and
                after["observer_ref"] == coverage["observer_ref"] and
                after["observed_at"] == transaction["committed_at"], "rsvp_storage_binding")
        require(after["claim"]["presence"] == "present" and
                int(after["claim"]["version"]) > 0 and
                after["claim"]["value_sha256"] == canonical_digest(projection), "rsvp_storage_digest")
        seen.add(after_ref)
