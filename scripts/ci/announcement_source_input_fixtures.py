"""Fabricated RESTRICTED source claims; no real feed, row, consent or parser proof."""

import copy
import hashlib

from announcement_provenance_fixtures import at, envelope
from announcement_scheduler_fixtures import scheduler_fixture, repin_scheduler
from announcement_runtime_evidence import canonical_digest


def sha(text):
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def repin_source_inputs(receipt, records):
    plan = records["runtime-source-plan"]["data"]
    for field in ("stored_feed", "body", "selection"):
        pin = plan[field]
        pin["sha256"] = canonical_digest(records[pin["ref"]]["data"])
    records["runtime-input-plan"]["data"]["source_plan_sha256"] = canonical_digest(plan)
    repin_scheduler(receipt, records)


def source_input_fixture():
    receipt, records, now = scheduler_fixture()

    def add(ref, kind, data):
        records[ref] = envelope(ref, kind, copy.deepcopy(data))
        return {"ref": ref, "sha256": canonical_digest(data)}

    target = {"origin": "https://fixture.example.test", "path": "/feed.xml"}
    item = {"key": "fixture-item-one", "title": "Fixture item", "url": "https://fixture.example.test/item",
            "published_at": at("00:00:00")}
    body = ('<?xml version="1.0"?><rss version="2.0"><channel><title>Fixture</title>'
            '<link>https://fixture.example.test/</link><description>Inert fixture</description>'
            '<item><guid>fixture-item-one</guid><title>Fixture item</title>'
            '<link>https://fixture.example.test/item</link></item></channel></rss>')
    inputs = records["runtime-input-plan"]["data"]
    inputs["feed"]["item_key"] = sha(item["key"])
    records["key-delivery"]["data"]["item_key"] = sha(item["key"])
    fixtures = receipt["fixtures"]
    fixtures.update(source_content_sha256=sha(body), item_content_sha256=canonical_digest(item))
    records["item-ref"]["data"].update(source_content_sha256=sha(body),
                                    item_content_sha256=canonical_digest(item))
    # Keep all resource identity/state/ownership bindings coherent after the
    # source and delivery PK changes; these remain untrusted synthetic claims.
    for ref in ("source", "delivery"):
        entry = next(x for x in receipt["resources"]["entries"] if x["ref"] == ref)
        if ref == "source":
            entry["identity"].update(origin_identity_sha256=sha(target["origin"]),
                path_sha256=sha(target["path"]), content_sha256=sha(body))
        else:
            entry["identity"]["key_sha256"] = canonical_digest(records["key-delivery"]["data"])
        binding = next(x for x in records["runtime-identity-map"]["data"]["bindings"]
                       if x["resource_ref"] == ref)
        binding["identity"] = copy.deepcopy(entry["identity"])
        for state_ref in ("before-" + ref, "after-" + ref, "isolation-" + ref):
            records[state_ref]["data"]["identity_sha256"] = canonical_digest(entry["identity"])
    plan = {"candidate": receipt["scope"]["candidate"], "source_ref": "source", "feed_ref": "feed",
        "item_ref": "item-ref", "target": target, "maximum_response_bytes": 4096,
        "maximum_wire_attempts": 3, "redirects": "deny", "content_encoding": "identity",
        "stored_feed": add("stored-source", "stored_feed_source", {
            "feed_ref": "feed", "key_ref": "key-feed", "guild_id": "200", "channel_id": "500",
            "source_url": target["origin"] + target["path"], "kind": "rss", "enabled": True,
            "observed_at": at("00:02:00"), "observer_ref": "resource-observer",
            "served_ref": "served-before"}),
        "body": add("source-body", "source_body", {"source_ref": "source", "target": target,
            "body_utf8": body, "byte_count": len(body.encode("utf-8")), "sha256": sha(body)}),
        "selection": add("selected-item", "source_selection", {"source_ref": "source",
            "body_sha256": sha(body), "item_ref": "item-ref", "item": item, "nonce_ref": "nonce-ref",
            "nonce": sha("fixture-relay\0" + sha(item["key"]))[:24],
            "post_content_sha256": sha("New feed item: **Fixture item**\n" + item["url"])})}
    add("runtime-source-plan", "source_plan_content", plan)
    repin_source_inputs(receipt, records)
    return receipt, records, now
