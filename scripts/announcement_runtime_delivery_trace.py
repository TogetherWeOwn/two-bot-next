"""Pinned offline delivery trace, NOT stored-row or no-repost enforcement proof.

Connects the terminal accounting model to declared case/transport/DB operations.
Partial pending/delivered row projections do not prove complete stored values,
claim-token fencing, SQL results, history, atomicity or actual readback. Missing
or uncertain evidence refuses; runtime admission remains unconditionally HOLD.
"""

from announcement_runtime_delivery import FeedDeliveryAccounting
from announcement_runtime_evidence import canonical_digest
from announcement_runtime_feed_exchange import check_feed_exchange_claims
from announcement_runtime_inputs import INPUT_PLAN_REF
from announcement_runtime_provenance import _index
from announcement_runtime_resources import PLAN_REF
from announcement_runtime_schema import require, utc
from announcement_runtime_source_inputs import SOURCE_PLAN_REF


def check_delivery_trace_claims(receipt, evidence, now):
    """Compare a fresh claim/post/completion/replay declaration with its ledgers.

    Replay requires a separately budgeted delivery DB read, not merely an idle
    poll, history miss or a copied successful response. No delivery write may
    intervene through that read except the one fresh claim and completion. This
    scoped comparison does not qualify later cleanup/restoration or prove that a
    claimed row projection is an actual result of the named database operation.
    """
    check_feed_exchange_claims(receipt, evidence, now)
    source = evidence.resolve(SOURCE_PLAN_REF, "source_plan_content")
    pin = source["delivery_trace"]
    trace = evidence.resolve(pin["ref"], "feed_delivery_trace")
    require(canonical_digest(trace) == pin["sha256"], "delivery_trace_content")
    inputs = evidence.resolve(INPUT_PLAN_REF, "input_plan_content")
    slots = evidence.resolve(PLAN_REF, "resource_plan_content")["slots"]
    selection = evidence.resolve(source["selection"]["ref"], "source_selection")
    keys = {entry["resource_ref"]: entry["key"]["ref"] for entry in inputs["keys"]}
    require(trace["key_ref"] == keys[slots["delivery_ref"]] and
            trace["nonce_ref"] == selection["nonce_ref"], "delivery_trace_scope")
    exchange = evidence.resolve(source["post_exchange"]["ref"], "feed_post_exchange")
    message_id = exchange["response"]["message"]["id"]
    observations = receipt["observations"]
    effects = _index(observations["actual_effects"], "operation_id", "operation_replay")
    coverage = observations["coverage"][0]
    feed_cases = {case["phase"]: case for case in observations["cases"]
                  if case["input_ref"] == receipt["fixtures"]["scheduler_plan_ref"]}
    model = FeedDeliveryAccounting(receipt["attempt"]["run"], trace["key_ref"],
                                   trace["nonce_ref"], exchange["operation_id"])
    common = {"run": receipt["attempt"]["run"], "key_ref": trace["key_ref"],
              "nonce_ref": trace["nonce_ref"], "claim_ref": trace["claim_ref"]}
    methods = {"claim": ("delivery_ref", "db_insert"),
               "wire_attempt": ("feed_channel_ref", "feed_post"),
               "accepted": ("feed_channel_ref", "feed_post"),
               "committed": ("delivery_ref", "db_cas"),
               "replay": ("delivery_ref", "db_read")}
    first_sequence, last_sequence = None, None
    last_time, prior_sequence = None, None
    write_ids = set()
    delivered = {"state": "delivered", "nonce": selection["nonce"], "message_id": message_id}
    for event in trace["events"]:
        kind = event["kind"]
        effect = effects.get(event["operation_id"])
        slot, method = methods[kind]
        case = feed_cases["replay" if kind == "replay" else "on"]
        require(effect is not None and effect["resource_ref"] == slots[slot] and
                effect["method"] == method and
                effect["operation_id"] in case["actual_effect_refs"], "delivery_trace_operation")
        observed = utc(event["observed_at"])
        expected_time = (exchange["response"]["received_at"] if kind == "accepted"
                         else effect["observed_at"])
        require(event["observed_at"] == expected_time and
                event["observer_ref"] == coverage["observer_ref"] and
                (last_time is None or last_time <= observed), "delivery_trace_observation")
        # Acceptance and wire attempt name the same operation. All other model
        # events must advance the full-window sequence, even at equal timestamps.
        if kind != "accepted":
            require(prior_sequence is None or prior_sequence < effect["sequence"],
                    "delivery_trace_order")
            prior_sequence = effect["sequence"]
        last_time = observed
        declared = {"kind": kind, **common}
        if kind == "claim":
            require(event["row"] == {"state": "pending", "nonce": selection["nonce"]} and
                    trace["claim_ref"] == effect["operation_id"], "delivery_trace_claim")
            first_sequence = effect["sequence"]
            write_ids.add(effect["operation_id"])
        elif kind == "wire_attempt":
            declared["operation_id"] = effect["operation_id"]
        elif kind == "accepted":
            require(event["operation_id"] == exchange["operation_id"] and
                    event["message_id"] == message_id, "delivery_trace_message")
            declared.update(operation_id=effect["operation_id"], message_id=event["message_id"])
        else:
            require(event["row"] == delivered, "delivery_trace_row")
            declared["message_id"] = event["row"]["message_id"]
            if kind == "committed":
                write_ids.add(effect["operation_id"])
            else:
                last_sequence = effect["sequence"]
        model.observe(declared)
    model.require_complete()
    require(first_sequence is not None and last_sequence is not None,
            "delivery_trace_incomplete")
    writes = {effect["operation_id"] for effect in effects.values()
              if effect["resource_ref"] == slots["delivery_ref"] and
              effect["method"] != "db_read" and
              first_sequence <= effect["sequence"] <= last_sequence}
    require(writes == write_ids, "delivery_trace_interference")
