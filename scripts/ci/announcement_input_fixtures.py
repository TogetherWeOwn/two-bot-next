"""Fabricated RESTRICTED input claims, not resource allocation or consent proof.

These examples still omit scheduler/transitive inventories and actual stored
cases/effect/telemetry/lifecycle evidence. They cannot qualify any runtime.
"""

import copy

from announcement_resource_fixtures import resource_fixture, repin_resources, repin_authority
from announcement_provenance_fixtures import at, envelope
from announcement_runtime_evidence import canonical_digest
from announcement_runtime_schema import COMMAND_GATES


def repin_inputs(receipt, records):
    plan = records["runtime-input-plan"]["data"]
    for binding in plan["keys"]:
        pin = binding["key"]
        pin["sha256"] = canonical_digest(records[pin["ref"]]["data"])
    for field in ("consent", "interactions"):
        pin = plan[field]
        pin["sha256"] = canonical_digest(records[pin["ref"]]["data"])
    records["runtime-resource-plan"]["data"]["input_plan_sha256"] = canonical_digest(plan)
    repin_resources(receipt, records)
    repin_authority(receipt, records)


def input_fixture():
    receipt, records, now = resource_fixture()

    def add(ref, kind, data):
        records[ref] = envelope(ref, kind, copy.deepcopy(data))

    keys = {
        "feed": {"table": "feed_relays", "id": "fixture-relay"},
        "delivery": {"table": "feed_deliveries", "feed_id": "fixture-relay", "item_key": "item-one"},
        "setting": {"table": "guild_settings", "guild_id": "200", "key": "TWO_ANNOUNCEMENTS"},
        "rsvp": {"table": "event_rsvps", "guild_id": "200", "event_id": "600", "user_id": "300"},
        "rsvp-audit": {"table": "announcements_audit_log", "id": "rsvp:909"},
    }
    bindings = []
    for ref, key in keys.items():
        key_ref = "key-" + ref
        add(key_ref, "literal_db_key", key)
        digest = canonical_digest(key)
        entry = next(x for x in receipt["resources"]["entries"] if x["ref"] == ref)
        entry["identity"]["key_sha256"] = digest
        binding = next(x for x in records["runtime-identity-map"]["data"]["bindings"]
                       if x["resource_ref"] == ref)
        binding["identity"] = copy.deepcopy(entry["identity"])
        for state_ref in ("before-" + ref, "after-" + ref, "isolation-" + ref):
            records[state_ref]["data"]["identity_sha256"] = canonical_digest(entry["identity"])
        bindings.append({"resource_ref": ref, "key": {"ref": key_ref, "sha256": digest}})
    interactions, cases = [], []
    example = receipt["observations"]["cases"][0]
    for index, (command, phase) in enumerate([
            *((name, "off") for name in sorted(COMMAND_GATES)), ("rsvp", "on")], 1):
        case_id = phase + "-" + command
        interactions.append({"case_id": case_id, "command": command, "phase": phase,
            "interaction_id": str(900 + index), "application_id": "100",
            "actor_ref": "actor", "channel_ref": "control-channel", "event_ref": "event",
            "request_sha256": "a" * 64, "ack_count": 1, "edit_count": int(phase == "on"),
            "followup_count": 0, "ephemeral": True,
            "maximum_wire_attempts": 1 + int(phase == "on")})
        case = copy.deepcopy(example)
        case.update(case_id=case_id, phase=phase, input_ref="interaction-plan-ref",
                    resource_refs=["actor", "control-channel", "event"])
        cases.append(case)
    receipt["observations"]["cases"] = cases
    receipt["fixtures"]["case_ids"] = [x["case_id"] for x in cases]
    # Other partial fixtures deliberately leave exchange resolution to its own
    # checker; this pin is not silently consumed as complete response evidence.
    add("interaction-plan-ref", "interaction_plan", {"interactions": interactions,
        "exchanges": {"ref": "interaction-exchanges", "sha256": "a" * 64}})
    add("consent-ref", "test_consent", {"actor_ref": "actor", "actor_id": "300",
        "application_id": "100", "guild_id": "200", "execution_ref": "execution-ref",
        "nonce_ref": "one-shot-nonce", "case_ids": [x["case_id"] for x in interactions],
        "granted_at": at("00:00:00"), "not_before": at("00:00:00"),
        "expires_at": at("00:40:00"), "outcome": "granted"})
    add("runtime-input-plan", "input_plan_content", {"keys": bindings,
        "scheduler_plan_sha256": "a" * 64, "source_plan_sha256": "b" * 64,
        "setting_plan_sha256": "c" * 64,
        "consent": {"ref": "consent-ref", "sha256": "a" * 64},
        "interactions": {"ref": "interaction-plan-ref", "sha256": "b" * 64},
        "feed": {"resource_ref": "feed", "guild_id": "200", "channel_id": "500",
                 "source_ref": "source", "item_key": "item-one", "nonce_ref": "nonce-ref"},
        "rsvp": {"resource_ref": "rsvp", "status": "going",
                 "interaction_id": "909", "audit_ref": "rsvp-audit"}})
    budgets = records["runtime-resource-plan"]["data"]["operation_budgets"]
    for budget in budgets:
        if budget["resource_ref"] == "control-channel" and budget["method"] == "interaction_ack":
            budget.update(expected_count=9, maximum_count=9)
    entry = next(x for x in receipt["resources"]["entries"] if x["ref"] == "control-channel")
    entry.update(expected_count=10, maximum_count=10)
    for ledger in ("expected_effects", "actual_effects"):
        effects = receipt["observations"][ledger]
        ack = next(x for x in effects if x["method"] == "interaction_ack")
        for number in range(7):
            operation = copy.deepcopy(ack)
            operation.update(operation_id="additional-ack-" + str(number), sequence=len(effects) + 1)
            effects.append(operation)
    repin_inputs(receipt, records)
    return receipt, records, now
