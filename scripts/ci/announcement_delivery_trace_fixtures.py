"""SYNTHETIC delivery trace claims, not stored-row, SQL or no-repost proof.

Built on feed_exchange_fixture() (real fabricated source, scheduler, telemetry,
case and exchange claims, never mocked). One feed_delivery_trace record names the
five modeled events (claim, wire attempt, accepted, committed, replay) and binds
them to the actual delivery/post operations. The row projections are partial
hand-written claims; no value is read from a database and no claim token, SQL
result, history or atomicity is modeled. This is a partial consistency graph, not
a full DB lifecycle: transition_fixture() is deliberately NOT called, so state
chains and ownership witnesses stay unconsumed.

Two ledger changes make the replay representable. A BUDGETED delivery db_read
(own transaction, no transition refs) follows the commit inside the replay case,
and the delivery cleanup (db_delete_owned, its transition observed_at and its
transaction committed_at) moves after that read. The record adds four sidecars
(trace, read transaction, pending and delivered row states) to the unchanged
256-record packet bound. The trace's mandatory storage object carries full
column projections (claim token as a digest only) whose digests the two row
states pin; the delivery transitions chain baseline, pending, delivered, cleanup.
Ownership and authority chains stay unbuilt.
"""

import copy
import hashlib

from announcement_case_fixtures import resequence
from announcement_feed_exchange_fixtures import (
    EXCHANGE, feed_exchange_fixture, post_operation, repin_feed_exchange,
)
from announcement_provenance_fixtures import at, envelope
from announcement_resource_fixtures import repin_authority, repin_resources
from announcement_runtime_evidence import canonical_digest
from announcement_runtime_source_inputs import SOURCE_PLAN_REF
from announcement_telemetry_fixtures import binding

TRACE = "feed-delivery-trace"
READ_OPERATION = "budget-delivery-db_read-0"
READ_TRANSACTION = "transaction-" + READ_OPERATION
REPLAY_AT = "00:10:30"
CLEANUP_AT = "00:14:30"
CLAIM_TOKEN_SHA256 = hashlib.sha256(b"synthetic-delivery-claim-token").hexdigest()
SQL_NULL = {"kind": "sql_null"}
KINDS = ("claim", "wire_attempt", "accepted", "committed", "replay")


def delivery_effect(receipt, method):
    """The one actual delivery-resource effect with this method."""
    return next(x for x in receipt["observations"]["actual_effects"]
                if x["resource_ref"] == "delivery" and x["method"] == method)


def set_effect(receipt, operation, **fields):
    """Rewrite one operation in both ledgers."""
    for ledger in ("actual_effects", "expected_effects"):
        for effect in receipt["observations"][ledger]:
            if effect["operation_id"] == operation:
                effect.update(copy.deepcopy(fields))


def move_delivery_write(receipt, records, method, clock):
    """Move one delivery write's effect, transition claim and transaction time.

    Only observed_at/committed_at change (the transition's state records are
    placeholders in this partial graph). The ledger is then resequenced.
    """
    effect = delivery_effect(receipt, method)
    operation = effect["operation_id"]
    records[effect["db_transition_refs"][0]]["data"]["claim"]["observed_at"] = at(clock)
    records["transaction-" + operation]["data"]["committed_at"] = at(clock)
    set_effect(receipt, operation, observed_at=at(clock))
    resequence(receipt, records)


def add_delivery_read(receipt, records):
    """Add one BUDGETED delivery db_read in its own transaction, in feed-replay.

    Entry counts/allowed operations, the plan budget and the derived resource and
    authority pins follow (as add_rsvp_read does for the RSVP resource). A read
    has no transition. Returns the operation id.
    """
    entry = next(x for x in receipt["resources"]["entries"] if x["ref"] == "delivery")
    entry["allowed_operations"].append("db_read")
    entry["expected_count"] += 1
    entry["maximum_count"] += 1
    records["runtime-resource-plan"]["data"]["operation_budgets"].append(
        {"ref": "budget-delivery-db_read", "resource_ref": "delivery", "method": "db_read",
         "expected_count": 1, "maximum_count": 1})
    repin_resources(receipt, records)
    repin_authority(receipt, records)
    observations = receipt["observations"]
    read = {"operation_id": READ_OPERATION, "sequence": len(observations["actual_effects"]) + 1,
            "resource_ref": "delivery", "method": "db_read", "payload_sha256": "a" * 64,
            "nonce_ref": [], "attempted_count": 1, "accepted_count": 1,
            "platform_id_refs": [], "db_transition_refs": [], "observed_at": at(REPLAY_AT),
            "served_ref": "served-one", "outcome": "accepted"}
    for ledger in ("expected_effects", "actual_effects"):
        observations[ledger].append(copy.deepcopy(read))
    case = next(x for x in observations["cases"] if x["case_id"] == "feed-replay")
    for field in ("expected_effect_refs", "actual_effect_refs"):
        case[field].append(READ_OPERATION)
    records[READ_TRANSACTION] = envelope(READ_TRANSACTION, "db_transaction", {
        "transaction_id": "%08x-0000-4000-8000-000000000000" % 0xde11,
        "observer_ref": observations["coverage"][0]["observer_ref"],
        "effects": [binding(receipt, read)], "transition_refs": [],
        "committed_at": at(REPLAY_AT), "outcome": "committed"})
    coverage = observations["coverage"][0]
    coverage["transaction_receipt_refs"].append(READ_TRANSACTION)
    coverage["last_sequence"] = len(observations["actual_effects"])
    resequence(receipt, records)
    return READ_OPERATION


def delivery_entry(receipt):
    return next(x for x in receipt["resources"]["entries"] if x["ref"] == "delivery")


def claim_state_ref(receipt):
    """Sidecar ref of the pending (claimed) row state; distinct from baseline/final."""
    return "state-" + delivery_effect(receipt, "db_insert")["operation_id"]


def completion_state_ref(receipt):
    """Sidecar ref of the delivered (completed) row state."""
    return "state-" + delivery_effect(receipt, "db_cas")["operation_id"]


def build_storage(receipt, records):
    """Full column projections, from the literal delivery key and the ledgers.

    Rows carry the claim token only as a digest; nothing here is a SQL result.
    """
    slots = records["runtime-resource-plan"]["data"]["slots"]
    keys = {x["resource_ref"]: x["key"]["ref"]
            for x in records["runtime-input-plan"]["data"]["keys"]}
    key = records[keys[slots["delivery_ref"]]]["data"]
    nonce = records["selected-item"]["data"]["nonce"]
    message_id = records[EXCHANGE]["data"]["response"]["message"]["id"]
    seen = delivery_effect(receipt, "db_insert")["observed_at"]
    done = delivery_effect(receipt, "db_cas")["observed_at"]
    base = {"feed_id": key["feed_id"], "item_key": key["item_key"], "nonce": nonce,
            "first_seen_at": seen}
    claimed = {**base, "state": "pending", "message_id": copy.deepcopy(SQL_NULL),
               "delivered_at": copy.deepcopy(SQL_NULL), "claim_token_sha256": CLAIM_TOKEN_SHA256,
               "claimed_at": seen}
    completed = {**base, "state": "delivered", "message_id": message_id, "delivered_at": done,
                 "claim_token_sha256": copy.deepcopy(SQL_NULL), "claimed_at": copy.deepcopy(SQL_NULL)}
    return {"claim_token_sha256": CLAIM_TOKEN_SHA256, "claimed": claimed,
            "completed": completed, "replay": copy.deepcopy(completed)}


def bind_delivery_states(receipt, records):
    """Add the two row-state sidecars and chain the three delivery transitions.

    Baseline -> pending (version 1) -> delivered (version 2) -> cleanup. Only
    the delivery transitions change; no ownership or authority chain is built,
    so the graph stays partial.
    """
    entry = delivery_entry(receipt)
    observer = receipt["observations"]["coverage"][0]["observer_ref"]
    storage = records[TRACE]["data"]["storage"]
    insert, cas = delivery_effect(receipt, "db_insert"), delivery_effect(receipt, "db_cas")
    delete = delivery_effect(receipt, "db_delete_owned")
    pending_ref, delivered_ref = claim_state_ref(receipt), completion_state_ref(receipt)
    for ref, version, row, effect in ((pending_ref, "1", storage["claimed"], insert),
                                      (delivered_ref, "2", storage["completed"], cas)):
        records[ref] = envelope(ref, "resource_state", {
            "claim": {"ref": ref, "presence": "present", "version": version,
                      "value_sha256": canonical_digest(row)},
            "resource_ref": entry["ref"], "identity_sha256": canonical_digest(entry["identity"]),
            "owner_ref": entry["owner"], "observer_ref": observer,
            "observed_at": effect["observed_at"]})
    for effect, before, after in ((insert, entry["before_state_ref"], pending_ref),
                                  (cas, pending_ref, delivered_ref),
                                  (delete, delivered_ref, None)):
        claim = records[effect["db_transition_refs"][0]]["data"]["claim"]
        claim["before_state_ref"] = before
        if after is not None:
            claim["intended_after_state_ref"] = after
            claim["actual_after_state_ref"] = after


def build_trace(receipt, records):
    """Write the five-event claim from the current fabricated ledgers."""
    slots = records["runtime-resource-plan"]["data"]["slots"]
    keys = {x["resource_ref"]: x["key"]["ref"]
            for x in records["runtime-input-plan"]["data"]["keys"]}
    selection = records["selected-item"]["data"]
    exchange = records[EXCHANGE]["data"]
    observer = receipt["observations"]["coverage"][0]["observer_ref"]
    claim, post = delivery_effect(receipt, "db_insert"), post_operation(receipt)
    complete, read = delivery_effect(receipt, "db_cas"), delivery_effect(receipt, "db_read")
    pending = {"state": "pending", "nonce": selection["nonce"]}
    delivered = {"state": "delivered", "nonce": selection["nonce"],
                 "message_id": exchange["response"]["message"]["id"]}

    def event(kind, effect, observed_at, **fields):
        return {"kind": kind, "operation_id": effect["operation_id"],
                "observed_at": observed_at, "observer_ref": observer, **fields}

    return {"key_ref": keys[slots["delivery_ref"]], "nonce_ref": selection["nonce_ref"],
            "claim_ref": claim["operation_id"], "storage": build_storage(receipt, records),
            "events": [
                event("claim", claim, claim["observed_at"], row=pending),
                event("wire_attempt", post, post["observed_at"]),
                event("accepted", post, exchange["response"]["received_at"],
                      message_id=delivered["message_id"]),
                event("committed", complete, complete["observed_at"], row=copy.deepcopy(delivered)),
                event("replay", read, read["observed_at"], row=copy.deepcopy(delivered))]}


def repin_delivery_states(receipt, records):
    """Refresh only the two derived full-row state value digests."""
    storage = records[TRACE]["data"].get("storage") if TRACE in records else None
    if type(storage) is not dict:
        return
    for ref, name in ((claim_state_ref(receipt), "claimed"),
                      (completion_state_ref(receipt), "completed")):
        state = records.get(ref)
        if state is not None and name in storage and state["data"]["claim"]["presence"] == "present":
            state["data"]["claim"]["value_sha256"] = canonical_digest(storage[name])


def repin_delivery_trace(receipt, records, *, trace=True, storage=True, **repin):
    """Refresh the source plan's delivery_trace pin, then the feed-exchange chain.

    trace=False deliberately leaves the pin stale. storage=False leaves the two
    full-row state digests stale. Semantic fields (refs, times, owners, versions)
    are never touched, so a mutation reaches the trace checker instead of a stale
    digest. repin accepts repin_feed_exchange's payload/pin switches.
    """
    if storage:
        repin_delivery_states(receipt, records)
    if trace and TRACE in records:
        records[SOURCE_PLAN_REF]["data"]["delivery_trace"]["sha256"] = canonical_digest(
            records[TRACE]["data"])
    repin_feed_exchange(receipt, records, **repin)


def delivery_trace_fixture():
    """Return (receipt, {ref: sidecar}, now). Fresh deep objects each call."""
    receipt, records, now = feed_exchange_fixture()
    add_delivery_read(receipt, records)
    move_delivery_write(receipt, records, "db_delete_owned", CLEANUP_AT)
    records[TRACE] = envelope(TRACE, "feed_delivery_trace", build_trace(receipt, records))
    bind_delivery_states(receipt, records)
    repin_delivery_trace(receipt, records)
    return receipt, records, now
