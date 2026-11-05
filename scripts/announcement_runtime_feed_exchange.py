"""Restricted feed post/response claims, NOT transport or persistence proof.

Only the already modeled one-post ASCII RSS profile is compared. No request is
sent, retry budget added, message read fetched or uncertain outcome reconciled.
The projected response cannot authenticate Discord or prove the platform's full
message contents; all-path enforcement and independent evidence remain HOLD.
"""

import hashlib

from announcement_runtime_cases import check_case_effect_claims
from announcement_runtime_evidence import canonical_digest
from announcement_runtime_provenance import _index
from announcement_runtime_resources import PLAN_REF
from announcement_runtime_schema import _validate, enum, parse_bounded_json, require, utc
from announcement_runtime_source_inputs import (
    SOURCE_PLAN_REF, check_source_input_claims, rss_post_content,
)


MENTIONS = {"parse": [], "roles": [], "users": [], "replied_user": False}


def check_feed_exchange_claims(receipt, evidence, now):
    """Compare the source-bound post, ledger, accepted ID and response projection.

    This is a consistency predicate, not a receipt producer or independent
    verifier. Unknown/failed/blocked effects already refuse upstream. A response
    loss, ambiguous history or repost cannot be converted into success here.
    """
    check_source_input_claims(receipt, evidence, now)
    check_case_effect_claims(receipt, evidence, now)
    plan = evidence.resolve(SOURCE_PLAN_REF, "source_plan_content")
    pin = plan["post_exchange"]
    exchange = evidence.resolve(pin["ref"], "feed_post_exchange")
    require(canonical_digest(exchange) == pin["sha256"], "feed_exchange_content")
    selection = evidence.resolve(plan["selection"]["ref"], "source_selection")
    slots = evidence.resolve(PLAN_REF, "resource_plan_content")["slots"]
    entries = _index(receipt["resources"]["entries"], "ref", "resource_inventory")
    channel = entries[slots["feed_channel_ref"]]["identity"]["object_id"]
    application = entries[slots["application_ref"]]["identity"]["application_id"]
    observations = receipt["observations"]
    effects = _index(observations["actual_effects"], "operation_id", "operation_replay")
    posts = [effect for effect in effects.values() if effect["method"] == "feed_post"]
    require(len(posts) == 1 and exchange["operation_id"] == posts[0]["operation_id"],
            "feed_exchange_operation")
    post = posts[0]
    cases = [case for case in observations["cases"]
             if post["operation_id"] in case["actual_effect_refs"]]
    require(len(cases) == 1 and cases[0]["phase"] == "on", "feed_exchange_case")
    case = cases[0]
    require(exchange["origin"] == "https://discord.com" and
            exchange["http_method"] == "POST" and
            exchange["path"] == f"/channels/{channel}/messages", "feed_exchange_route")
    raw = exchange["body_utf8"].encode("utf-8")
    body = parse_bounded_json(raw)
    content = rss_post_content(selection["item"])
    # C: core/feeds.rs plan_post; discord/executor.rs send_message_components;
    # discord/message_safety.rs sanitize_message. No components/embeds/poll or
    # attachments are part of this post. The raw serializer emits all four
    # mention-policy fields, unlike optional interaction-builder fields.
    expected = {"content": content, "nonce": selection["nonce"],
                "enforce_nonce": True, "allowed_mentions": MENTIONS}
    _validate(body, {"content": enum(content), "nonce": enum(selection["nonce"]),
                     "enforce_nonce": enum(True), "allowed_mentions": {
                         key: enum(value) for key, value in MENTIONS.items()}})
    require(body == expected and hashlib.sha256(raw).hexdigest() == post["payload_sha256"],
            "feed_exchange_payload")
    response = exchange["response"]
    # SERVICE_ID syntax alone admits 20-digit values beyond Discord's u64
    # range. C validates the destination snowflake and mutation receipt id.
    require(all(0 < int(identifier) <= 2**64 - 1 for identifier in
                (channel, application, response["message"]["id"],
                 response["message"]["channel_id"], response["message"]["author_id"])),
            "feed_exchange_identity")
    coverage = observations["coverage"][0]
    platforms = [evidence.resolve(ref, "platform_effect")
                 for ref in coverage["platform_receipt_refs"]]
    accepted = [record for record in platforms if record["operation_id"] == post["operation_id"]]
    require(len(accepted) == 1, "feed_exchange_platform")
    platform = accepted[0]
    complete = [effects[ref] for ref in case["actual_effect_refs"]
                if effects[ref]["resource_ref"] == slots["delivery_ref"] and
                effects[ref]["method"] == "db_cas"]
    require(len(complete) == 1 and
            utc(post["observed_at"]) <= utc(response["received_at"]) <=
            utc(complete[0]["observed_at"]) and
            utc(response["received_at"]) < utc(case["ended_at"]) and
            response["received_at"] == platform["observed_at"] and
            response["observer_ref"] == coverage["observer_ref"], "feed_exchange_observation")
    require(response["http_status"] == 200, "feed_exchange_status")
    # This is the closed supported POST-response projection, not a new GET,
    # full Discord response schema or independent destination-history read.
    require(response["message"] == {"id": post["platform_id_refs"][0],
        "channel_id": channel, "author_id": application, "content": content,
        "nonce": selection["nonce"], "flags": 0, "mentions": [], "mention_roles": [],
        "mention_everyone": False, "attachments": []}, "feed_exchange_message")
