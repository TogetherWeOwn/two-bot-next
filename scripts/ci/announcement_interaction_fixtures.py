"""SYNTHETIC token-free interaction exchange claims, not Discord transport proof.

Built on case_fixture(). One aggregate interaction_exchanges record holds the
eight disabled-command refusals and one first RSVP (ack, then edit of the SAME
original message). Request projections, raw body text and readbacks are
hand-written and unauthenticated; no callback secret, signature, real response
or stored RSVP value exists. The upstream scheduler, telemetry and case claims
are real fixtures, never mocked. The exchange record adds one sidecar to the
unchanged 256-record packet bound.

The aggregate also carries a mandatory schema-valid storage claim: the RSVP row
and audit row projections of the single shared RSVP+audit transaction.
interaction_fixture() adds NO state sidecars; its two insert transitions keep
their placeholder after-state refs and it does not satisfy the storage checker.
rsvp_storage_fixture() adds two distinct present-state sidecars that pin the
canonical projection digests and the transaction's commit timestamp. All of it
is hand-written claims; no SQL ran and no stored value was read. Cleanup (the
RSVP delete) and the audit's non-restored final state are NOT qualified here:
those transitions keep their typed placeholders.
"""

import copy
import hashlib
import json

from announcement_case_fixtures import case_fixture
from announcement_input_fixtures import repin_inputs
from announcement_provenance_fixtures import at, envelope
from announcement_runtime_evidence import canonical_digest
from announcement_telemetry_fixtures import repin_telemetry

EXCHANGES = "interaction-exchanges"
PLAN = "interaction-plan-ref"
DISABLED = ("Announcements are disabled on this server. Ask a server admin to enable them "
            "in the bot configuration — this is a host setting, not a Discord role.")
STATUS = "going"
SAVED = "RSVP saved: " + STATUS + "."
TRANSACTION = "transaction-rsvp-audit"
RESPONDED_AT = "00:07:50"
SQL_NULL = {"kind": "sql_null"}
MENTIONS = {"parse": [], "users": [], "roles": [], "replied_user": False}


def raw(value):
    """Compact JSON text, as a serializer would emit it."""
    return json.dumps(value, separators=(",", ":"))


def ack_body(positive):
    if positive:
        return raw({"type": 5, "data": {"flags": 64, "allowed_mentions": copy.deepcopy(MENTIONS)}})
    return raw({"type": 4, "data": {"flags": 64, "content": DISABLED,
                                    "allowed_mentions": copy.deepcopy(MENTIONS)}})


def saved(status=STATUS):
    return "RSVP saved: " + status + "."


def edit_body(status=STATUS):
    return raw({"content": saved(status), "allowed_mentions": copy.deepcopy(MENTIONS)})


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


def rsvp_inserts(receipt, records):
    """The one actual RSVP-row insert and the one audit-row insert effect."""
    slots = records["runtime-resource-plan"]["data"]["slots"]
    return tuple(next(x for x in receipt["observations"]["actual_effects"]
                      if x["resource_ref"] == slots[slot] and x["method"] == "db_insert")
                 for slot in ("rsvp_ref", "rsvp_audit_ref"))


def rsvp_state_refs(receipt, records):
    """Sidecar refs of the RSVP and audit present states; distinct from baselines."""
    return tuple("state-" + effect["operation_id"] for effect in rsvp_inserts(receipt, records))


def build_storage(receipt, records, status):
    """Row and audit projections from the literal keys, as C constructs them.

    discord/rsvp.rs builds the response timestamp after the pre-reads; core/rsvp.rs
    derives audit id 'rsvp:<interaction id>', SQL-null reason, event target, the
    same outcome and the same timestamp. Nothing here is a SQL result.
    """
    slots = records["runtime-resource-plan"]["data"]["slots"]
    keys = {x["resource_ref"]: records[x["key"]["ref"]]["data"]
            for x in records["runtime-input-plan"]["data"]["keys"]}
    key, audit_key = keys[slots["rsvp_ref"]], keys[slots["rsvp_audit_ref"]]
    responded = at(RESPONDED_AT)
    row = {"guild_id": key["guild_id"], "event_id": key["event_id"],
           "user_id": key["user_id"], "status": status, "responded_at": responded}
    audit = {"id": audit_key["id"], "guild_id": row["guild_id"], "actor_id": row["user_id"],
             "action": "event.rsvp", "target_key": row["event_id"], "outcome": status,
             "reason": copy.deepcopy(SQL_NULL), "created_at": responded}
    return {"case_id": "on-rsvp", "transaction_ref": TRANSACTION, "rsvp": row, "audit": audit}


def bind_rsvp_states(receipt, records):
    """Add two distinct present-state sidecars and point the two insert transitions at them.

    Each state has version 1, the exact resource identity/owner/observer, the shared
    transaction's commit timestamp and the canonical projection digest. Only the
    two insert transitions' after refs change; cleanup stays a placeholder.
    """
    storage = records[EXCHANGES]["data"]["storage"]
    observer = receipt["observations"]["coverage"][0]["observer_ref"]
    committed = records[TRANSACTION]["data"]["committed_at"]
    entries = {x["ref"]: x for x in receipt["resources"]["entries"]}
    for effect, ref, name in zip(rsvp_inserts(receipt, records),
                                 rsvp_state_refs(receipt, records), ("rsvp", "audit")):
        entry = entries[effect["resource_ref"]]
        records[ref] = envelope(ref, "resource_state", {
            "claim": {"ref": ref, "presence": "present", "version": "1",
                      "value_sha256": canonical_digest(storage[name])},
            "resource_ref": entry["ref"], "identity_sha256": canonical_digest(entry["identity"]),
            "owner_ref": entry["owner"], "observer_ref": observer, "observed_at": committed})
        claim = records[effect["db_transition_refs"][0]]["data"]["claim"]
        claim["intended_after_state_ref"] = claim["actual_after_state_ref"] = ref


def repin_rsvp_states(receipt, records):
    """Refresh ONLY the two projection value digests from the storage aggregate."""
    storage = records[EXCHANGES]["data"].get("storage") if EXCHANGES in records else None
    if type(storage) is not dict:
        return
    for ref, name in zip(rsvp_state_refs(receipt, records), ("rsvp", "audit")):
        state = records.get(ref)
        if state is not None and type(storage.get(name)) is dict and \
                state["data"]["claim"]["presence"] == "present":
            state["data"]["claim"]["value_sha256"] = canonical_digest(storage[name])


def repin_interactions(receipt, records, *, bodies=True, requests=True, pin=True,
                       states=True):
    """Refresh ONLY derived hashes after a coherent edit.

    bodies: effect/platform payload hashes follow each raw body_utf8.
    requests: plan request hashes follow each request projection.
    pin: the plan's exchange content pin follows the aggregate record.
    states: the RSVP/audit present-state value digests follow the storage claims.
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
    if states:
        repin_rsvp_states(receipt, records)
    if pin:
        plan["exchanges"] = {"ref": EXCHANGES, "sha256": canonical_digest(content)}
    repin_telemetry(receipt, records)
    repin_inputs(receipt, records)


def interaction_fixture(status=STATUS):
    """Return (receipt, {ref: sidecar}, now). Fresh deep objects each call.

    status coherently selects the RSVP status in the input plan, request option,
    edit body and readback, storage projections and state digests.
    """
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
    assert inputs["rsvp"]["status"] == STATUS
    inputs["rsvp"]["status"] = status
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
            body = ack_body(positive) if is_ack else edit_body(status)
            responses.append({"operation_id": effect["operation_id"],
                "origin": "https://discord.com",
                "route": "interaction_callback" if is_ack else "original_response",
                "http_method": "POST" if is_ack else "PATCH",
                "interaction_id": request["interaction_id"],
                "application_id": request["application_id"], "body_utf8": body,
                "readback": {"message_id": message_id,
                             "application_id": request["application_id"],
                             "channel_id": channel, "flags": 192 if positive and is_ack else 64,
                             "content": "" if positive and is_ack else saved(status) if positive else DISABLED}})
        eligibility = []
        for ref in case["actual_effect_refs"]:
            effect = effects[ref]
            if effect["method"] not in ("member_get", "event_get"):
                continue
            member = effect["method"] == "member_get"
            eligibility.append({"operation_id": ref, "observer_ref": observer,
                "origin": "https://discord.com", "http_method": "GET",
                "path": f"/guilds/{application['guild_id']}/members/{actor}" if member else
                    f"/guilds/{application['guild_id']}/scheduled-events/{event}",
                "received_at": effect["observed_at"], "http_status": 200,
                "body_utf8": raw({"user": {"id": actor}} if member else
                    {"id": event, "guild_id": application["guild_id"], "status": 2})})
            set_payload(receipt, records, ref, hashlib.sha256(b"").hexdigest())
        exchanges.append({"case_id": interaction["case_id"], "observer_ref": observer,
                          "received_at": ack["observed_at"], "request": request,
                          "responses": responses, "eligibility_reads": eligibility})
        if positive:
            set_platform_id(receipt, records, ack["operation_id"], message_id)
    records[EXCHANGES] = {"schema": records[PLAN]["schema"], "ref": EXCHANGES,
                          "kind": "interaction_exchanges", "run": copy.deepcopy(records[PLAN]["run"]),
                          "data": {"storage": build_storage(receipt, records, status),
                                   "exchanges": exchanges}}
    repin_interactions(receipt, records)
    return receipt, records, now


def rsvp_storage_fixture(status=STATUS):
    """interaction_fixture(status) plus the two present-state sidecars (+2 records)."""
    receipt, records, now = interaction_fixture(status)
    bind_rsvp_states(receipt, records)
    repin_interactions(receipt, records)
    return receipt, records, now
