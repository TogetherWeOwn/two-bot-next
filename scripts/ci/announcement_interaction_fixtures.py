"""SYNTHETIC token-free interaction exchange claims, not Discord transport proof.

Built on case_fixture(). One aggregate interaction_exchanges record holds the
eight disabled-command refusals and one first RSVP (ack, then edit of the SAME
original message). Request projections, raw body text and readbacks are
hand-written and unauthenticated; no callback secret, signature, real response
or stored RSVP value exists. The upstream scheduler, telemetry and case claims
are real fixtures, never mocked. The new exchange record adds one sidecar to the
unchanged 256-record packet bound.
"""

import copy
import hashlib
import json

from announcement_case_fixtures import case_fixture
from announcement_input_fixtures import repin_inputs
from announcement_runtime_evidence import canonical_digest
from announcement_telemetry_fixtures import repin_telemetry

EXCHANGES = "interaction-exchanges"
PLAN = "interaction-plan-ref"
DISABLED = ("Announcements are disabled on this server. Ask a server admin to enable them "
            "in the bot configuration — this is a host setting, not a Discord role.")
STATUS = "going"
SAVED = "RSVP saved: " + STATUS + "."
MENTIONS = {"parse": [], "users": [], "roles": [], "replied_user": False}


def raw(value):
    """Compact JSON text, as a serializer would emit it."""
    return json.dumps(value, separators=(",", ":"))


def ack_body(positive):
    if positive:
        return raw({"type": 5, "data": {"flags": 64, "allowed_mentions": copy.deepcopy(MENTIONS)}})
    return raw({"type": 4, "data": {"flags": 64, "content": DISABLED,
                                    "allowed_mentions": copy.deepcopy(MENTIONS)}})


def edit_body():
    return raw({"content": SAVED, "allowed_mentions": copy.deepcopy(MENTIONS)})


def set_payload(receipt, records, operation, digest):
    """Rewrite one operation's payload hash in both ledgers and its platform claim."""
    for ledger in ("actual_effects", "expected_effects"):
        for effect in receipt["observations"][ledger]:
            if effect["operation_id"] == operation:
                effect["payload_sha256"] = digest
    platform = records.get("platform-" + operation)
    if platform is not None:
        platform["data"]["payload_sha256"] = digest


def set_platform_id(receipt, records, operation, identifier):
    """Rewrite one operation's platform id in both ledgers and its platform claim."""
    for ledger in ("actual_effects", "expected_effects"):
        for effect in receipt["observations"][ledger]:
            if effect["operation_id"] == operation:
                effect["platform_id_refs"] = [identifier]
    records["platform-" + operation]["data"]["platform_ids"] = [identifier]


def repin_interactions(receipt, records, *, bodies=True, requests=True, pin=True):
    """Refresh ONLY derived hashes after a coherent edit.

    bodies: effect/platform payload hashes follow each raw body_utf8.
    requests: plan request hashes follow each request projection.
    pin: the plan's exchange content pin follows the aggregate record.
    Semantic fields (ids, readbacks, times, observers, routes) are untouched, so
    a mutation reaches the interaction checker instead of a stale digest. The
    telemetry and input chains are then re-derived so upstream checks stay green.
    """
    content = records[EXCHANGES]["data"]
    plan = records[PLAN]["data"]
    known = {x["operation_id"] for x in receipt["observations"]["actual_effects"]}
    if bodies:
        for exchange in content["exchanges"]:
            for response in exchange["responses"]:
                if response["operation_id"] in known:
                    set_payload(receipt, records, response["operation_id"],
                                hashlib.sha256(response["body_utf8"].encode("utf-8")).hexdigest())
    if requests:
        planned = {x["case_id"]: x for x in plan["interactions"]}
        for exchange in content["exchanges"]:
            if exchange["case_id"] in planned:
                planned[exchange["case_id"]]["request_sha256"] = canonical_digest(exchange["request"])
    if pin:
        plan["exchanges"] = {"ref": EXCHANGES, "sha256": canonical_digest(content)}
    repin_telemetry(receipt, records)
    repin_inputs(receipt, records)


def interaction_fixture():
    """Return (receipt, {ref: sidecar}, now). Fresh deep objects each call."""
    receipt, records, now = case_fixture()
    observations = receipt["observations"]
    plan = records[PLAN]["data"]
    slots = records["runtime-resource-plan"]["data"]["slots"]
    entries = {x["ref"]: x for x in receipt["resources"]["entries"]}
    application = entries[slots["application_ref"]]["identity"]
    channel = entries[slots["control_channel_ref"]]["identity"]["object_id"]
    actor = entries[slots["actor_ref"]]["identity"]["object_id"]
    event = entries[slots["event_ref"]]["identity"]["object_id"]
    observer = observations["coverage"][0]["observer_ref"]
    inputs = records["runtime-input-plan"]["data"]
    status = inputs["rsvp"]["status"]
    assert status == STATUS
    effects = {x["operation_id"]: x for x in observations["actual_effects"]}
    cases = {x["case_id"]: x for x in observations["cases"]}
    exchanges = []
    for interaction in plan["interactions"]:
        case = cases[interaction["case_id"]]
        positive = interaction["phase"] == "on"
        operations = [effects[ref] for ref in case["actual_effect_refs"]
                      if effects[ref]["method"] in ("interaction_ack", "interaction_edit")]
        operations.sort(key=lambda effect: effect["sequence"])
        ack = operations[0]
        message_id = ack["platform_id_refs"][0]
        if positive:
            # Acknowledge and edit the same original message: one shared id.
            edit = operations[1]
            message_id = edit["platform_id_refs"][0]
        request = {"interaction_id": interaction["interaction_id"],
                   "application_id": application["application_id"],
                   "guild_id": application["guild_id"], "channel_id": channel,
                   "actor_id": actor, "command": interaction["command"],
                   "options": [{"name": "event-id", "value": event},
                               {"name": "status", "value": status}] if positive else []}
        responses = []
        for effect in operations:
            is_ack = effect["method"] == "interaction_ack"
            body = ack_body(positive) if is_ack else edit_body()
            responses.append({"operation_id": effect["operation_id"],
                "origin": "https://discord.com",
                "route": "interaction_callback" if is_ack else "original_response",
                "http_method": "POST" if is_ack else "PATCH",
                "interaction_id": request["interaction_id"],
                "application_id": request["application_id"], "body_utf8": body,
                "readback": {"message_id": message_id,
                             "application_id": request["application_id"],
                             "channel_id": channel, "flags": 192 if positive and is_ack else 64,
                             "content": "" if positive and is_ack else SAVED if positive else DISABLED}})
        exchanges.append({"case_id": interaction["case_id"], "observer_ref": observer,
                          "received_at": ack["observed_at"], "request": request,
                          "responses": responses})
        if positive:
            set_platform_id(receipt, records, ack["operation_id"], message_id)
    records[EXCHANGES] = {"schema": records[PLAN]["schema"], "ref": EXCHANGES,
                          "kind": "interaction_exchanges", "run": copy.deepcopy(records[PLAN]["run"]),
                          "data": {"exchanges": exchanges}}
    repin_interactions(receipt, records)
    return receipt, records, now
