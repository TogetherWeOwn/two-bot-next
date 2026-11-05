"""SYNTHETIC feed post/response projection claims, not Discord transport proof.

Built on case_fixture(source_input_fixture()). One feed_post_exchange record
holds the hand-written raw JSON body of the single RSS post and a projected
HTTP 200 message, content-bound by the source plan's post_exchange pin. Nothing
is sent, authenticated or read back from a platform; the response is a claim
agreeing with a fabricated ledger. The expected text is written here
independently of the production helper. The upstream source, scheduler,
telemetry and case claims are real fixtures, never mocked. The record adds one
sidecar to the unchanged 256-record packet bound.
"""

import copy
import hashlib
import json

from announcement_case_fixtures import case_fixture
from announcement_provenance_fixtures import envelope
from announcement_runtime_evidence import canonical_digest
from announcement_source_input_fixtures import (
    replace_source_body, repin_source_inputs, sha, source_input_fixture,
)
from announcement_telemetry_fixtures import repin_telemetry

EXCHANGE = "feed-post-exchange"
MENTIONS = {"parse": [], "roles": [], "users": [], "replied_user": False}


def raw(value):
    """Compact JSON text, as a serializer would emit it."""
    return json.dumps(value, separators=(",", ":"))


def post_content(item):
    """The independently written post text for the narrow ASCII RSS profile."""
    return "New feed item: **" + item["title"].replace("-", "\\-") + "**\n" + item["url"]


def post_body(content, nonce):
    return raw({"content": content, "nonce": nonce, "enforce_nonce": True,
                "allowed_mentions": copy.deepcopy(MENTIONS)})


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


def post_operation(receipt):
    """The one feed_post effect, whatever the exchange record currently claims."""
    return next(x for x in receipt["observations"]["actual_effects"]
                if x["method"] == "feed_post")


def repin_feed_exchange(receipt, records, *, payload=True, pin=True):
    """Refresh ONLY derived hashes after a coherent edit.

    payload: the post effect/platform payload hash follows the raw body_utf8.
    pin: the source plan's post_exchange pin follows the exchange record.
    Semantic fields (ids, route, content, times, observers, status) are never
    touched, so a mutation reaches the feed-exchange checker instead of a stale
    digest. Telemetry then the source/scheduler/input/authority chain are
    re-derived last so upstream checks stay green.
    """
    content = records.get(EXCHANGE)
    if payload and content is not None:
        digest = hashlib.sha256(content["data"]["body_utf8"].encode("utf-8")).hexdigest()
        set_payload(receipt, records, post_operation(receipt)["operation_id"], digest)
    repin_telemetry(receipt, records)
    repin_source_inputs(receipt, records, exchange=pin)


def rebind_item(receipt, records, *, title=None, url=None):
    """Coherently change the selected RSS item everywhere EXCEPT the exchange.

    Source body, selection, item hashes and post_content_sha256 follow the new
    item, so the source checker stays green and only the exchange text can drift.
    """
    selected = records["selected-item"]["data"]
    item = selected["item"]
    body = records["source-body"]["data"]["body_utf8"]
    if title is not None:
        body = body.replace("<title>" + item["title"] + "</title>", "<title>" + title + "</title>")
        item["title"] = title
    if url is not None:
        body = body.replace("<link>" + item["url"] + "</link>", "<link>" + url + "</link>")
        item["url"] = url
    digest = canonical_digest(item)
    receipt["fixtures"]["item_content_sha256"] = digest
    records["item-ref"]["data"]["item_content_sha256"] = digest
    selected["post_content_sha256"] = sha(post_content(item))
    replace_source_body(receipt, records, body)
    repin_feed_exchange(receipt, records)


def follow_item(receipt, records):
    """Rewrite the exchange body and message to the current selected item."""
    selected = records["selected-item"]["data"]
    content = post_content(selected["item"])
    data = records[EXCHANGE]["data"]
    data["body_utf8"] = post_body(content, selected["nonce"])
    data["response"]["message"]["content"] = content
    repin_feed_exchange(receipt, records)


def feed_exchange_fixture():
    """Return (receipt, {ref: sidecar}, now). Fresh deep objects each call."""
    receipt, records, now = case_fixture(source_input_fixture())
    observations = receipt["observations"]
    slots = records["runtime-resource-plan"]["data"]["slots"]
    entries = {x["ref"]: x for x in receipt["resources"]["entries"]}
    channel = entries[slots["feed_channel_ref"]]["identity"]["object_id"]
    application = entries[slots["application_ref"]]["identity"]["application_id"]
    post = post_operation(receipt)
    selected = records["selected-item"]["data"]
    content = post_content(selected["item"])
    platform = records["platform-" + post["operation_id"]]["data"]
    records[EXCHANGE] = envelope(EXCHANGE, "feed_post_exchange", {
        "operation_id": post["operation_id"], "origin": "https://discord.com",
        "http_method": "POST", "path": "/channels/" + channel + "/messages",
        "body_utf8": post_body(content, selected["nonce"]),
        "response": {"http_status": 200, "received_at": platform["observed_at"],
            "observer_ref": observations["coverage"][0]["observer_ref"],
            "message": {"id": post["platform_id_refs"][0], "channel_id": channel,
                "author_id": application, "content": content, "nonce": selected["nonce"],
                "flags": 0, "mentions": [], "mention_roles": [], "mention_everyone": False,
                "attachments": []}}})
    repin_feed_exchange(receipt, records)
    return receipt, records, now
