"""Pinned offline delivery trace, NOT SQL or no-repost enforcement proof.

Connects terminal accounting and restricted column projections to declared
case/transport/DB operations and state digests. A token fingerprint is not its
raw stored value or a CAS fence; projections do not authenticate SQL results,
history, atomicity or actual readback. Runtime admission remains HOLD.
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
    scoped comparison binds column projections to declared state digests, but
    does not qualify later cleanup/restoration or authenticate any SQL result.
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
    _check_storage(receipt, evidence, trace, effects, feed_cases["on"], selection, exchange, slots)


def _check_storage(receipt, evidence, trace, effects, case, selection, exchange, slots):
    """Compare the fresh-only column profile with its two declared write states.

    C's 0180_feeds.sql columns are represented, except the claim token is only a
    fingerprint. Digests below identify this closed projection, NOT raw SQL bytes
    or a database-owned version token. Replay is a claimed result of the already
    budgeted read, never a new read or permission to release/reclaim a row.
    """
    storage = trace["storage"]
    claimed, completed, replay = (storage[name] for name in ("claimed", "completed", "replay"))
    require(claimed["state"] == "pending" and
            completed["state"] == replay["state"] == "delivered", "delivery_storage_state")
    key = evidence.resolve(trace["key_ref"], "literal_db_key")
    require(key["table"] == "feed_deliveries", "delivery_storage_key")
    columns = {"feed_id": key["feed_id"], "item_key": key["item_key"],
               "nonce": selection["nonce"], "first_seen_at": claimed["claimed_at"]}
    sql_null = {"kind": "sql_null"}
    require(claimed == {**columns, "state": "pending", "message_id": sql_null,
        "delivered_at": sql_null, "claim_token_sha256": storage["claim_token_sha256"],
        "claimed_at": claimed["claimed_at"]}, "delivery_storage_claim")
    require(completed == {**columns, "state": "delivered",
        "message_id": exchange["response"]["message"]["id"],
        "delivered_at": completed["delivered_at"], "claim_token_sha256": sql_null,
        "claimed_at": sql_null} and replay == completed, "delivery_storage_completion")
    events = {event["kind"]: event for event in trace["events"]}
    claim_effect, completion_effect = (effects[events[name]["operation_id"]]
                                       for name in ("claim", "committed"))
    claim_at, completed_at = utc(claimed["claimed_at"]), utc(completed["delivered_at"])
    require(utc(case["started_at"]) <= claim_at <= utc(claim_effect["observed_at"]) and
            utc(exchange["response"]["received_at"]) <= completed_at <=
            utc(completion_effect["observed_at"]), "delivery_storage_time")
    resource = next(entry for entry in receipt["resources"]["entries"]
                    if entry["ref"] == slots["delivery_ref"])
    observer = receipt["observations"]["coverage"][0]["observer_ref"]
    prior_ref = resource["before_state_ref"]
    baseline = evidence.resolve(prior_ref, "resource_state")
    require(baseline["claim"]["presence"] == "absent" and
            utc(baseline["observed_at"]) <= claim_at, "delivery_storage_baseline")
    prior_version = 0
    created = {resource["before_state_ref"], resource["after_state_ref"]}
    for effect, projection in ((claim_effect, claimed), (completion_effect, completed)):
        transition = evidence.resolve(effect["db_transition_refs"][0], "db_transition")["claim"]
        ref = transition["actual_after_state_ref"]
        require(transition["before_state_ref"] == prior_ref and
                transition["intended_after_state_ref"] == ref and ref not in created,
                "delivery_storage_chain")
        state = evidence.resolve(ref, "resource_state")
        require(state["claim"]["ref"] == ref and
                state["resource_ref"] == resource["ref"] and
                state["identity_sha256"] == canonical_digest(resource["identity"]) and
                state["owner_ref"] == resource["owner"] and
                state["observer_ref"] == observer and
                state["observed_at"] == effect["observed_at"], "delivery_storage_binding")
        value = state["claim"]
        require(value["presence"] == "present", "delivery_storage_presence")
        require(int(value["version"]) > prior_version, "delivery_storage_version")
        require(value["value_sha256"] == canonical_digest(projection), "delivery_storage_digest")
        prior_ref, prior_version = ref, int(value["version"])
        created.add(ref)
