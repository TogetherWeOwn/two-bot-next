"""Restricted token-free interaction projections, NOT authenticated requests.

The narrow first-RSVP/raw-event-ID profile compares claimed wire body bytes and
original-message readback with source-backed ephemeral reply semantics. It does
not observe Discord, resolve callback secrets, verify actual stored RSVP values,
or establish that a projection faithfully represents a real request/response.
"""

from datetime import timedelta
import hashlib

from announcement_runtime_cases import check_case_effect_claims
from announcement_runtime_evidence import canonical_digest
from announcement_runtime_inputs import INPUT_PLAN_REF
from announcement_runtime_provenance import _index, _unique_refs
from announcement_runtime_resources import PLAN_REF
from announcement_runtime_schema import Integer, Text, _validate, enum, parse_bounded_json, require, utc
from announcement_runtime_source import CANDIDATE, CANDIDATE_TREE


# Immutable C: core/router.rs ANNOUNCEMENTS_DISABLED_REPLY; discord/rsvp.rs
# prepare/complete; core/rsvp.rs rsvp_saved_text_for_transition (first response).
DISABLED = ("Announcements are disabled on this server. Ask a server admin to enable them "
            "in the bot configuration — this is a host setting, not a Discord role.")
MENTIONS = {"parse": [], "users": [], "roles": [], "replied_user": False}


def _body(raw, method, positive, status):
    value = parse_bounded_json(raw.encode("utf-8"))
    # Only known empty mention fields may be omitted by a serializer. Unknown
    # fields, nulls, enabled reply mentions and nonempty parse/users/roles refuse.
    data = value.get("data") if type(value) is dict and method == "interaction_ack" else value
    require(type(data) is dict, "interaction_body")
    mentions = data.get("allowed_mentions")
    require(type(mentions) is dict and set(mentions) <= MENTIONS.keys() and
            "parse" in mentions, "interaction_mentions")
    for field, actual in mentions.items():
        _validate(actual, enum(False) if field == "replied_user" else enum([]))
    normalized = {**data, "allowed_mentions": MENTIONS}
    if method == "interaction_ack":
        _validate(value, {"type": Integer(4, 5), "data": {
            "flags": Integer(), "allowed_mentions": {field: enum(actual)
                for field, actual in mentions.items()},
            **({"content": Text(r"[\s\S]+")} if not positive else {})}})
        expected = {"flags": 64, "allowed_mentions": MENTIONS}
        if not positive:
            expected["content"] = DISABLED
        require(value["type"] == (5 if positive else 4) and normalized == expected,
                "interaction_ack_body")
    else:
        _validate(value, {"content": Text(r"[\s\S]+"), "allowed_mentions": {
            field: enum(actual) for field, actual in mentions.items()}})
        require(normalized == {"content": "RSVP saved: " + status + ".",
                               "allowed_mentions": MENTIONS}, "interaction_edit_body")


def check_interaction_exchange_claims(receipt, evidence, now):
    """Compare closed projections with exact case, operation and platform claims.

    Only off/final-off refusals and one first RSVP are supported here. Other
    negative/positive/replay profiles refuse; no new logical or retry budget is
    created. Input hashing is over this token-free projection, NOT the original
    Discord envelope. Stored transition correctness remains a separate check.
    """
    check_case_effect_claims(receipt, evidence, now)
    require(receipt["scope"]["candidate"] == {"commit": CANDIDATE, "tree": CANDIDATE_TREE},
            "interaction_source")
    plan = evidence.resolve(PLAN_REF, "resource_plan_content")
    inputs = evidence.resolve(INPUT_PLAN_REF, "input_plan_content")
    interactions = evidence.resolve(inputs["interactions"]["ref"], "interaction_plan")
    pin = interactions["exchanges"]
    content = evidence.resolve(pin["ref"], "interaction_exchanges")
    require(canonical_digest(content) == pin["sha256"], "interaction_exchange_content")
    exchanges = _index(content["exchanges"], "case_id", "interaction_exchange_inventory")
    selected = _index(interactions["interactions"], "case_id", "interaction_exchange_inventory")
    require(exchanges.keys() == selected.keys(), "interaction_exchange_inventory")
    cases = _index(receipt["observations"]["cases"], "case_id", "case_inventory")
    effects = _index(receipt["observations"]["actual_effects"], "operation_id", "operation_replay")
    resources = _index(receipt["resources"]["entries"], "ref", "resource_inventory")
    slots = plan["slots"]
    application = resources[slots["application_ref"]]["identity"]
    channel = resources[slots["control_channel_ref"]]["identity"]["object_id"]
    actor = resources[slots["actor_ref"]]["identity"]["object_id"]
    event = resources[slots["event_ref"]]["identity"]["object_id"]
    coverage = receipt["observations"]["coverage"][0]
    platforms = {record["operation_id"]: record for record in
        (evidence.resolve(ref, "platform_effect") for ref in coverage["platform_receipt_refs"])}
    used = []
    for case_id, exchange in exchanges.items():
        interaction, case = selected[case_id], cases[case_id]
        positive = interaction["phase"] == "on"
        require(interaction["phase"] in ("off", "on", "final_off"),
                "unsupported_interaction_exchange")
        request = exchange["request"]
        options = _index(request["options"], "name", "interaction_options")
        expected_options = {"event-id": {"name": "event-id", "value": event},
                            "status": {"name": "status", "value": inputs["rsvp"]["status"]}} if positive else {}
        require(options == expected_options, "interaction_options")
        require(request == {"interaction_id": interaction["interaction_id"],
            "application_id": application["application_id"], "guild_id": application["guild_id"],
            "channel_id": channel, "actor_id": actor, "command": interaction["command"],
            "options": request["options"]} and
            canonical_digest(request) == interaction["request_sha256"], "interaction_request")
        received = utc(exchange["received_at"])
        require(exchange["observer_ref"] == coverage["observer_ref"] and
                utc(case["started_at"]) <= received < utc(case["ended_at"]),
                "interaction_received")
        case_effects = [effects[ref] for ref in case["actual_effect_refs"]
                        if effects[ref]["method"] in ("interaction_ack", "interaction_edit")]
        responses = exchange["responses"]
        ids = [response["operation_id"] for response in responses]
        _unique_refs(ids)
        require(set(ids) == {effect["operation_id"] for effect in case_effects},
                "interaction_response_inventory")
        message_ids = []
        for response in responses:
            operation = response["operation_id"]
            effect = effects[operation]
            ack = effect["method"] == "interaction_ack"
            require(response["origin"] == "https://discord.com" and
                    response["route"] == ("interaction_callback" if ack else "original_response") and
                    response["http_method"] == ("POST" if ack else "PATCH") and
                    response["interaction_id"] == request["interaction_id"] and
                    response["application_id"] == request["application_id"], "interaction_route")
            raw = response["body_utf8"]
            require(hashlib.sha256(raw.encode("utf-8")).hexdigest() == effect["payload_sha256"],
                    "interaction_body_digest")
            _body(raw, effect["method"], positive, inputs["rsvp"]["status"])
            platform = platforms[operation]
            readback = response["readback"]
            require(platform["platform_ids"] == [readback["message_id"]] and
                    readback["application_id"] == request["application_id"] and
                    readback["channel_id"] == channel and
                    readback["flags"] == (192 if positive and ack else 64) and
                    readback["content"] == ("" if positive and ack else
                        "RSVP saved: " + inputs["rsvp"]["status"] + "." if positive else DISABLED),
                    "interaction_readback")
            require(received <= utc(effect["observed_at"]), "interaction_response_order")
            if ack:
                # Whole-second claims conservatively refuse a difference of 3
                # or more seconds. This bounds the attempted ACK only; coarse
                # timestamps do not prove actual Discord deadline compliance.
                require(utc(effect["observed_at"]) - received < timedelta(seconds=3),
                        "interaction_ack_deadline")
            message_ids.append(readback["message_id"])
            used.append(operation)
        require(len(set(message_ids)) == 1, "interaction_original_message")
    _unique_refs(used)
