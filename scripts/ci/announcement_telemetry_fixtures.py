"""SYNTHETIC telemetry correspondence claims, not observation or lifecycle proof.

Built on resource_fixture(). Its effects deliberately sit at ONE instant
(00:08:00) as accounting examples; that is NOT actual setup/cleanup lifecycle or
stored-case evidence and these fixtures preserve that disclaimer. Every wire,
platform, transaction and transition record here is hand-written, fabricated and
unauthenticated. State, authority and ownership refs inside transitions are
typed placeholders whose semantic resolution remains future lifecycle work.
The fixture window is exactly the attempt (00:00:00-00:24:00) and the
external-observer job is widened to cover it.
"""

import copy

from announcement_provenance_fixtures import at, envelope
from announcement_resource_fixtures import repin_authority, repin_resources, resource_fixture
from announcement_runtime_evidence import canonical_digest

PLATFORM_METHODS = ("feed_post", "interaction_ack", "interaction_edit",
                    "guild_commands_replace")
PLATFORM_ID_BASE = {"interaction_ack": 9000, "interaction_edit": 9100, "feed_post": 9200,
                    "guild_commands_replace": 9300}
OBSERVED = "00:08:00"
PLATFORM_OBSERVED = "00:08:01"


def _entries(receipt):
    return {x["ref"]: x for x in receipt["resources"]["entries"]}


def binding(receipt, effect):
    """Exact effect/resource-identity binding used by wire and DB records."""
    identity = _entries(receipt)[effect["resource_ref"]]["identity"]
    return {"operation_id": effect["operation_id"],
            "effect_sha256": canonical_digest(effect),
            "resource_identity_sha256": canonical_digest(identity)}


def _uuid(number):
    return "%08x-0000-4000-8000-000000000000" % number


def repin_telemetry(receipt, records):
    """Refresh ONLY derived hashes/window copies after a coherent effect edit.

    Semantic fields (outcomes, ids, nonces, observers, transitions, groupings)
    are never touched, so a mutation reaches its semantic predicate instead of
    being masked by a stale digest.
    """
    observations = receipt["observations"]
    effects = {x["operation_id"]: x for x in observations["actual_effects"]}
    entries = _entries(receipt)
    for record in records.values():
        data = record["data"]
        if record["kind"] in ("wire_attempt", "platform_effect"):
            effect = effects.get(data.get("operation_id"))
            if effect is not None:
                data["resource_identity_sha256"] = canonical_digest(
                    entries[effect["resource_ref"]]["identity"])
                if record["kind"] == "wire_attempt":
                    data["effect_sha256"] = canonical_digest(effect)
        elif record["kind"] == "db_transaction":
            for bound in data["effects"]:
                effect = effects.get(bound["operation_id"])
                if effect is not None:
                    bound.update(binding(receipt, effect))
    coverage = observations["coverage"][0]
    records[coverage["ref"]]["data"] = {
        "claim": copy.deepcopy(coverage),
        "operation_ids": [x["operation_id"] for x in observations["actual_effects"]]}


def telemetry_fixture(base=None):
    """Add fabricated telemetry to the default resource or supplied input graph."""
    receipt, records, now = resource_fixture() if base is None else base
    observations = receipt["observations"]
    entries = _entries(receipt)
    nonce = receipt["fixtures"]["nonce_ref"]
    counters = {}
    for effect in observations["actual_effects"]:
        method = effect["method"]
        if method in PLATFORM_METHODS:
            counters[method] = counters.get(method, 0) + 1
            effect["platform_id_refs"] = [str(PLATFORM_ID_BASE[method] + counters[method])]
        if method == "feed_post":
            effect["nonce_ref"] = [nonce]
        if method.startswith("db_") and method != "db_read":
            effect["db_transition_refs"] = ["transition-" + effect["operation_id"]]
    observations["expected_effects"] = copy.deepcopy(observations["actual_effects"])
    effects = observations["actual_effects"]

    # The original external observer job started at 00:00:10; telemetry needs the
    # job to span the ENTIRE attempt window, so widen it exactly to the attempt.
    records["job-observer"]["data"].update(started_at=at("00:00:00"), ended_at=at("00:24:00"))

    def add(ref, kind, data):
        records[ref] = envelope(ref, kind, copy.deepcopy(data))

    coverage = observations["coverage"][0]
    observer = coverage["observer_ref"]
    wire_refs, platform_refs, transaction_refs = [], [], []
    for effect in effects:
        operation = effect["operation_id"]
        if effect["method"].startswith("db_"):
            continue
        ref = "wire-" + operation
        add(ref, "wire_attempt", {**binding(receipt, effect), "observer_ref": observer,
            "observed_at": effect["observed_at"], "outcome": "accepted"})
        wire_refs.append(ref)
        if effect["method"] in PLATFORM_METHODS:
            ref = "platform-" + operation
            add(ref, "platform_effect", {"operation_id": operation,
                "resource_identity_sha256": binding(receipt, effect)["resource_identity_sha256"],
                "method": effect["method"], "payload_sha256": effect["payload_sha256"],
                "nonce_refs": copy.deepcopy(effect["nonce_ref"]),
                "platform_ids": copy.deepcopy(effect["platform_id_refs"]),
                "observer_ref": observer, "observed_at": at(PLATFORM_OBSERVED),
                "outcome": "accepted"})
            platform_refs.append(ref)

    slots = records["runtime-resource-plan"]["data"]["slots"]
    rsvp_pair = {x["operation_id"] for x in effects
                 if x["resource_ref"] in (slots["rsvp_ref"], slots["rsvp_audit_ref"])
                 and x["method"] == "db_insert"}
    groups, paired = [], []
    for effect in effects:
        if not effect["method"].startswith("db_"):
            continue
        if effect["operation_id"] in rsvp_pair:
            paired.append(effect)
        else:
            groups.append(("transaction-" + effect["operation_id"], [effect]))
    groups.append(("transaction-rsvp-audit", paired))
    for number, (ref, members) in enumerate(groups, 1):
        transitions = []
        for effect in members:
            resource = entries[effect["resource_ref"]]
            transition_ref = effect["db_transition_refs"][0]
            add(transition_ref, "db_transition", {"transaction_ref": ref,
                "method": effect["method"], "claim": {
                    "operation_id": effect["operation_id"], "owner": resource["owner"],
                    "resource_ref": resource["ref"],
                    "key_sha256": resource["identity"]["key_sha256"],
                    "before_state_ref": resource["before_state_ref"],
                    "intended_after_state_ref": resource["after_state_ref"],
                    "actual_after_state_ref": resource["after_state_ref"],
                    "expected_count": 1, "actual_count": 1,
                    "observed_at": effect["observed_at"],
                    "authority_ref": "execution-ref", "ownership_match_ref": "resource-owner",
                    "outcome": "matched"}})
            transitions.append(transition_ref)
        add(ref, "db_transaction", {"transaction_id": _uuid(number), "observer_ref": observer,
            "effects": [binding(receipt, x) for x in members], "transition_refs": transitions,
            "committed_at": at(OBSERVED), "outcome": "committed"})
        transaction_refs.append(ref)

    inventory = receipt["resources"]["inventory_manifest"]["sha256"]
    coverage.update(started_at=receipt["attempt"]["started_at"],
                    ended_at=receipt["attempt"]["ended_at"], first_sequence=1,
                    last_sequence=len(effects), enforcement_epoch="9",
                    source_inventory_sha256=inventory, wire_receipt_refs=wire_refs,
                    platform_receipt_refs=platform_refs,
                    transaction_receipt_refs=transaction_refs, result="complete")
    add(coverage["ref"], "telemetry_window", {"claim": coverage,
        "operation_ids": [x["operation_id"] for x in effects]})
    return receipt, records, now


def add_rsvp_read(receipt, records, transaction_ref="transaction-rsvp-audit"):
    """Add one BUDGETED rsvp db_read and bind it into an existing transaction.

    Updates the entry, plan budgets and authority scope claims, appends the read
    effect to both ledgers, then re-derives telemetry bindings. A read has no
    transition. Returns the new operation id.
    """
    entry = _entries(receipt)["rsvp"]
    entry["allowed_operations"].append("db_read")
    entry["expected_count"] += 1
    entry["maximum_count"] += 1
    plan = records["runtime-resource-plan"]["data"]
    plan["operation_budgets"].append({"ref": "budget-rsvp-db_read", "resource_ref": "rsvp",
        "method": "db_read", "expected_count": 1, "maximum_count": 1})
    repin_resources(receipt, records)
    repin_authority(receipt, records)
    observations = receipt["observations"]
    operation = "budget-rsvp-db_read-0"
    read = {"operation_id": operation, "sequence": len(observations["actual_effects"]) + 1,
            "resource_ref": "rsvp", "method": "db_read", "payload_sha256": "a" * 64,
            "nonce_ref": [], "attempted_count": 1, "accepted_count": 1,
            "platform_id_refs": [], "db_transition_refs": [],
            "observed_at": at(OBSERVED), "served_ref": "served-one", "outcome": "accepted"}
    for ledger in ("expected_effects", "actual_effects"):
        observations[ledger].append(copy.deepcopy(read))
    records[transaction_ref]["data"]["effects"].append(binding(receipt, read))
    coverage = observations["coverage"][0]
    coverage["last_sequence"] = len(observations["actual_effects"])
    repin_telemetry(receipt, records)
    return operation
