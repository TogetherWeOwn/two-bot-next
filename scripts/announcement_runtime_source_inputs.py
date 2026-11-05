"""Restricted offline source-content comparisons, never fetching or admission.

A body hash cannot prove immutable hosting or a real stored row. This layer
checks a deliberately narrow single-item ASCII RSS grammar against C's selection
rules; general XML/URL/Unicode behavior remains unsupported. Actual transport,
served-parser observations and external containment still need independent proof.
No public serializer, root, credential or network adapter exists here.
"""

import hashlib
import re

from announcement_runtime_evidence import canonical_digest
from announcement_runtime_inputs import INPUT_PLAN_REF
from announcement_runtime_resources import PLAN_REF
from announcement_runtime_scheduler import check_scheduler_graph_claims
from announcement_runtime_schema import require, utc


SOURCE_PLAN_REF = "runtime-source-plan"

# A closed fixture grammar, not a general XML parser. C feeds.rs:1109-1239
# selects this one unprefixed item, with no attribute shadowing, repeated fields,
# fallback key/title/date, entity decoding, namespace or URL normalization.
# Bounded disjoint character classes cannot contain markup or escape sequences.
RSS_FIXTURE = re.compile(
    r'<\?xml version="1\.0"\?><rss version="2\.0"><channel>'
    r'<title>[A-Za-z0-9][A-Za-z0-9 -]{0,127}</title>'
    r'<link>(?P<channel_url>https://[a-z0-9./_~-]{1,2048})</link>'
    r'<description>[A-Za-z0-9][A-Za-z0-9 -]{0,127}</description>'
    r'<item><guid>(?P<key>[A-Za-z0-9][A-Za-z0-9:._-]{0,255})</guid>'
    r'<title>(?P<title>[A-Za-z0-9][A-Za-z0-9 -]{0,127})</title>'
    r'<link>(?P<url>https://[a-z0-9./_~-]{1,2048})</link>'
    r'<pubDate>(?P<published_at>[0-9]{4}-[0-9]{2}-[0-9]{2}T'
    r'[0-9]{2}:[0-9]{2}:[0-9]{2}Z)</pubDate></item></channel></rss>'
)


def _sha(text):
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def _target(target):
    # No parsing/resolving of arbitrary URLs. Canonical DNS spelling and an exact
    # query-free path are a narrower supported profile, not a safe-egress proof.
    host = target["origin"][len("https://"):]
    labels = host.split(".")
    require(len(host) <= 253 and len(labels) >= 2 and
            all(not x.startswith("xn--") and
                re.fullmatch(r"[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?", x)
                for x in labels) and
            re.fullmatch(r"[a-z][a-z0-9-]*", labels[-1]) is not None,
            "source_target")
    path = target["path"]
    require(path.startswith("/") and not path.startswith("//") and
            all(x not in (".", "..") for x in path.split("/")) and
            "//" not in path and len((target["origin"] + path).encode("utf-8")) <= 2048,
            "source_target")
    return target["origin"] + path


def check_source_input_claims(receipt, evidence, now):
    """Compare content-bound source/row/item/nonce claims; return no capability.

    This is not a complete source verifier. The single-item fixture grammar
    rejects divergent selection claims, but never establishes immutable hosting
    or that served C actually fetched/parsed these bytes. A coherent fabricated
    graph can never pass runtime_preflight().
    """
    check_scheduler_graph_claims(receipt, evidence, now)
    inputs = evidence.resolve(INPUT_PLAN_REF, "input_plan_content")
    resources = evidence.resolve(PLAN_REF, "resource_plan_content")
    plan = evidence.resolve(SOURCE_PLAN_REF, "source_plan_content")
    require(canonical_digest(plan) == inputs["source_plan_sha256"], "source_content")
    fixtures, attempt = receipt["fixtures"], receipt["attempt"]
    slots = resources["slots"]
    require(plan["candidate"] == receipt["scope"]["candidate"] and
            plan["feed_ref"] == slots["feed_ref"] and
            plan["source_ref"] == slots["source_ref"] and
            plan["item_ref"] == fixtures["item_ref"], "source_fixture")
    entries = {x["ref"]: x for x in receipt["resources"]["entries"]}
    identity = entries[slots["source_ref"]]["identity"]
    url = _target(plan["target"])
    require(identity["origin_identity_sha256"] == _sha(plan["target"]["origin"]) and
            identity["path_sha256"] == _sha(plan["target"]["path"]), "source_target_binding")

    def content(pin, kind):
        value = evidence.resolve(pin["ref"], kind)
        require(canonical_digest(value) == pin["sha256"], "source_content")
        return value

    stored = content(plan["stored_feed"], "stored_feed_source")
    key_binding = next(x for x in inputs["keys"] if x["resource_ref"] == slots["feed_ref"])
    key = evidence.resolve(key_binding["key"]["ref"], "literal_db_key")
    require(stored["feed_ref"] == slots["feed_ref"] and
            stored["key_ref"] == key_binding["key"]["ref"] and
            stored["guild_id"] == inputs["feed"]["guild_id"] and
            stored["channel_id"] == inputs["feed"]["channel_id"] and
            stored["source_url"] == url, "stored_source")
    require(stored["kind"] == "rss", "unsupported_source_profile")
    observed = utc(stored["observed_at"])
    require(utc(attempt["started_at"]) <= observed <= utc(receipt["rollout"]["started_at"]),
            "stored_source_time")
    require(stored["served_ref"] == receipt["lifecycle"]["served_before"]["ref"],
            "stored_source_served")
    source = evidence.resolve(receipt["authority"]["source_ref"], "authority_source")
    verifier = evidence.resolve(source["verifier_ref"], "authority_consumer")
    observer = evidence.resolve(stored["observer_ref"], "observer")
    require(observer["principal"] == verifier["principal"] and
            observer["provisioning_ref"] == verifier["provisioning_ref"] and
            observer["job_ref"] in attempt["job_refs"], "source_observer")
    job = evidence.resolve(observer["job_ref"], "job")
    producer = evidence.resolve(job["producer_ref"], "producer")
    require(job["source"] in ("independent_verifier", "external_observer") and
            job["conclusion"] == "success" and
            job["head"] == receipt["control"]["version"]["commit"] and
            job["producer_ref"] in attempt["producer_refs"] and
            utc(attempt["started_at"]) <= utc(job["started_at"]) <= observed <=
            utc(job["ended_at"]) <= utc(attempt["ended_at"]) and
            producer["principal"] == verifier["principal"] and
            producer["workflow"] == receipt["control"]["workflow"], "source_observer")

    body = content(plan["body"], "source_body")
    require(body["source_ref"] == slots["source_ref"] and body["target"] == plan["target"] and
            body["sha256"] == _sha(body["body_utf8"]) == fixtures["source_content_sha256"] and
            body["byte_count"] == len(body["body_utf8"].encode("utf-8")) <=
            plan["maximum_response_bytes"], "source_body")
    selected = content(plan["selection"], "source_selection")
    require(selected["source_ref"] == slots["source_ref"] and
            selected["body_sha256"] == body["sha256"] and
            selected["item_ref"] == fixtures["item_ref"] and
            canonical_digest(selected["item"]) == fixtures["item_content_sha256"] and
            selected["nonce_ref"] == fixtures["nonce_ref"], "source_selection")
    # C feeds.rs:1770-1784,1797-1831 hashes the trimmed raw item key, then
    # derives the nonce from relay ID + NUL + that HASH, not the raw item key.
    # The closed nonempty ASCII key/title subset needs no JS/Rust trim emulation.
    item = selected["item"]
    require(item["title"] == item["title"].strip(), "unsupported_source_profile")
    parts = item["url"].removeprefix("https://").split("/", 1)
    require(len(parts) == 2, "source_target")
    _target({"origin": "https://" + parts[0], "path": "/" + parts[1]})
    item_key = _sha(item["key"])
    require(inputs["feed"]["item_key"] == item_key and
            selected["nonce"] == _sha(key["id"] + "\0" + item_key)[:24], "source_nonce")
    title = item["title"].replace("-", "\\-")
    post = "New feed item: **" + title + "**\n" + item["url"]
    require(len(post) <= 2000 and selected["post_content_sha256"] == _sha(post), "source_post")
    # Full document matching rejects hidden extra items/fields, DTD/entities,
    # CDATA, comments, processing instructions, namespaces and malformed XML
    # without invoking an XML engine or resolving any external input.
    parsed = RSS_FIXTURE.fullmatch(body["body_utf8"])
    require(parsed is not None, "unsupported_rss_fixture")
    fields = parsed.groupdict()
    channel_parts = fields.pop("channel_url").removeprefix("https://").split("/", 1)
    require(len(channel_parts) == 2, "source_target")
    _target({"origin": "https://" + channel_parts[0], "path": "/" + channel_parts[1]})
    require(fields == item, "source_parser_selection")
    budgets = [x for x in resources["operation_budgets"]
               if x["resource_ref"] == slots["source_ref"] and x["method"] == "source_get"]
    require(len(budgets) == 1 and budgets[0]["expected_count"] ==
            budgets[0]["maximum_count"] == plan["maximum_wire_attempts"], "source_budget")
    require(all(x["maximum_count"] == 0 or x["resource_ref"] == slots["source_ref"]
                for x in resources["operation_budgets"] if x["method"] == "source_get"),
            "source_budget")
