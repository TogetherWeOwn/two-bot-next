"""Hermetic feed-exchange adversaries; not Discord, send or stored-data proof.

GitHub Actions is the suite's CI of record; focused reproductions are disclosed
separately. Passing shows only that a fabricated raw POST body and projected
HTTP 200 message agree with a fabricated source, case and effect ledger.
Nothing is sent, observed or authenticated, no response-loss, retry or repost
path is modeled, and runtime_preflight stays unsupported. Semantic negatives
repin every derived hash (payload digest, exchange pin, telemetry, source and
input chain) so the feed-exchange checker, not a stale digest, refuses. Where an
intake schema refuses first the test asserts only a Refusal; exact codes are
asserted only where predictable.
"""

import copy
import json
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import announcement_feed_exchange_fixtures as fixtures
from announcement_feed_exchange_fixtures import (
    EXCHANGE, feed_exchange_fixture, follow_item, post_body, post_operation, raw,
    rebind_item, repin_feed_exchange, set_payload, set_platform_id,
)
from announcement_case_fixtures import case_fixture
from announcement_provenance_fixtures import evidence_index
from announcement_runtime_evidence import canonical_digest
from announcement_runtime_feed_exchange import check_feed_exchange_claims
from announcement_runtime_policy import runtime_preflight
from announcement_runtime_schema import Refusal
from announcement_runtime_source_inputs import SOURCE_PLAN_REF, check_source_input_claims
from announcement_source_input_fixtures import source_input_fixture


def exchange(records):
    return records[EXCHANGE]["data"]


def response(records):
    return exchange(records)["response"]


def message(records):
    return response(records)["message"]


def body(records):
    return json.loads(exchange(records)["body_utf8"])


def with_body(text):
    def mutate(receipt, records):
        exchange(records)["body_utf8"] = text
    return mutate


def with_fields(change):
    """Mutate the decoded body and re-serialize it as compact JSON."""
    def mutate(receipt, records):
        value = body(records)
        change(value)
        exchange(records)["body_utf8"] = raw(value)
    return mutate


def operation_of(receipt, method):
    return next(x for x in receipt["observations"]["actual_effects"] if x["method"] == method)


def move_platform(receipt, records, clock):
    """Move the accepted platform claim and the response to one instant."""
    stamp = "2026-01-01T" + clock + "Z"
    records["platform-" + post_operation(receipt)["operation_id"]]["data"]["observed_at"] = stamp
    response(records)["received_at"] = stamp


class FeedExchange(unittest.TestCase):
    def check(self, receipt, records, now):
        return check_feed_exchange_claims(receipt, evidence_index(records), now)

    def reject(self, mutate, code=None, **repin):
        receipt, records, now = feed_exchange_fixture()
        mutate(receipt, records)
        repin_feed_exchange(receipt, records, **repin)
        with self.assertRaises(Refusal) as caught:
            self.check(receipt, records, now)
        if code is not None:
            self.assertEqual(code, caught.exception.code)

    def accept(self, mutate, **repin):
        receipt, records, now = feed_exchange_fixture()
        mutate(receipt, records)
        repin_feed_exchange(receipt, records, **repin)
        self.assertIsNone(self.check(receipt, records, now))


class Coherence(FeedExchange):
    def test_baseline_is_bounded_returns_none_and_is_not_runtime_authority(self):
        receipt, records, now = feed_exchange_fixture()
        self.assertLessEqual(len(records), 256)
        for name in ("cases", "actual_effects", "expected_effects"):
            self.assertLessEqual(len(receipt["observations"][name]), 256)
        index = evidence_index(records)
        self.assertIsNone(check_feed_exchange_claims(receipt, index, now))
        # Inherited authority sidecars stay unconsumed: complete packets still refuse.
        with self.assertRaisesRegex(Refusal, "unreferenced_evidence$"):
            index.require_all_used()
        self.assertFalse(receipt["promotable"])
        with self.assertRaisesRegex(Refusal, "unsupported"):
            runtime_preflight()

    def test_one_pinned_exchange_binds_the_single_post_effect_and_platform_claim(self):
        receipt, records, _ = feed_exchange_fixture()
        self.assertEqual(1, sum(x["kind"] == "feed_post_exchange" for x in records.values()))
        post = post_operation(receipt)
        plan = records[SOURCE_PLAN_REF]["data"]
        self.assertEqual({"ref": EXCHANGE, "sha256": canonical_digest(exchange(records))},
                         plan["post_exchange"])
        self.assertEqual(post["operation_id"], exchange(records)["operation_id"])
        self.assertEqual(post["payload_sha256"], records["platform-" + post["operation_id"]]
                         ["data"]["payload_sha256"])
        self.assertEqual(post["platform_id_refs"], [message(records)["id"]])
        self.assertEqual(records["platform-" + post["operation_id"]]["data"]["observed_at"],
                         response(records)["received_at"])
        self.assertEqual({"content", "nonce", "enforce_nonce", "allowed_mentions"}, set(body(records)))

    def test_fixture_is_synthetic_and_calls_do_not_share_state(self):
        self.assertIn("SYNTHETIC", fixtures.__doc__)
        first = feed_exchange_fixture()
        first[1][EXCHANGE]["data"]["response"]["message"].clear()
        first[0]["observations"]["actual_effects"].clear()
        fresh = feed_exchange_fixture()
        self.assertEqual(10, len(message(fresh[1])))
        self.assertEqual(copy.deepcopy(feed_exchange_fixture()[1]), feed_exchange_fixture()[1])

    def test_default_case_and_source_fixtures_keep_their_partial_shape(self):
        _, records, _ = case_fixture()
        self.assertNotIn(SOURCE_PLAN_REF, records)
        self.assertNotIn(EXCHANGE, records)
        receipt, records, now = source_input_fixture()
        pin = records[SOURCE_PLAN_REF]["data"]["post_exchange"]
        self.assertEqual(EXCHANGE, pin["ref"])
        self.assertNotIn(EXCHANGE, records)
        # Source-only qualification is unchanged: the pin is not resolved there.
        self.assertIsNone(check_source_input_claims(receipt, evidence_index(records), now))
        # case_fixture(seed) adds only the seed's claims to the default graph.
        self.assertIn(SOURCE_PLAN_REF, case_fixture(source_input_fixture())[1])

    def test_equivalent_json_spelling_is_accepted_when_rehashed(self):
        def reorder(receipt, records):
            value = body(records)
            mentions = value["allowed_mentions"]
            text = json.dumps({"allowed_mentions": {key: mentions[key] for key in
                               ("replied_user", "users", "roles", "parse")},
                               "enforce_nonce": True, "nonce": value["nonce"],
                               "content": value["content"]}, indent=2)
            exchange(records)["body_utf8"] = text
        self.accept(reorder)

    def test_response_may_arrive_at_the_delivery_cas_instant(self):
        # The delivery CAS is the last case effect (00:08:10); equality is allowed.
        self.accept(lambda r, s: move_platform(r, s, "00:08:10"))

    def test_post_follows_a_coherently_rebound_source_item(self):
        # A hyphen is escaped like the candidate, so a new source item needs a new post.
        def rebound(receipt, records):
            rebind_item(receipt, records, title="Fixture-item")
            follow_item(receipt, records)
        receipt, records, now = feed_exchange_fixture()
        rebound(receipt, records)
        self.assertIn("**Fixture\\-item**", message(records)["content"])
        self.assertIsNone(self.check(receipt, records, now))
        # The unescaped spelling is not what C posts.
        response(records)["message"]["content"] = "New feed item: **Fixture-item**\n" + \
            "https://fixture.example.test/item"
        exchange(records)["body_utf8"] = post_body(message(records)["content"],
                                                   message(records)["nonce"])
        repin_feed_exchange(receipt, records)
        with self.assertRaises(Refusal) as caught:
            self.check(receipt, records, now)
        self.assertEqual("enum", caught.exception.code)


class Pin(FeedExchange):
    def test_source_plan_pin_is_mandatory_and_closed(self):
        for number, change in enumerate((lambda pin, plan: plan.pop("post_exchange"),
                       lambda pin, plan: pin.update(extra="x"),
                       lambda pin, plan: pin.pop("sha256"),
                       lambda pin, plan: pin.update(sha256="G" * 64),
                       lambda pin, plan: pin.update(sha256="a" * 63),
                       lambda pin, plan: pin.update(ref="Upper"))):
            with self.subTest(change=number):
                receipt, records, now = feed_exchange_fixture()
                plan = records[SOURCE_PLAN_REF]["data"]
                change(plan["post_exchange"], plan)
                with self.assertRaises(Refusal):
                    self.check(receipt, records, now)

    def test_missing_unresolved_or_wrong_kind_exchange_refuses(self):
        self.reject(lambda r, s: s.pop(EXCHANGE), "unresolved_reference", payload=False)
        self.reject(lambda r, s: s[SOURCE_PLAN_REF]["data"]["post_exchange"].update(
            ref="other-exchange"), "unresolved_reference")
        # A real record of another kind cannot stand in for the exchange.
        self.reject(lambda r, s: s[SOURCE_PLAN_REF]["data"]["post_exchange"].update(
            ref="selected-item"), "reference_kind")
        self.reject(lambda r, s: s[EXCHANGE].update(kind="source_selection"))
        self.reject(lambda r, s: s[EXCHANGE].update(kind="document"))

    def test_stale_or_wrong_digest_pin_refuses(self):
        self.reject(lambda r, s: response(s)["message"].update(content="changed"),
                    "feed_exchange_content", pin=False)
        self.reject(lambda r, s: s[SOURCE_PLAN_REF]["data"]["post_exchange"].update(
            sha256="e" * 64), "feed_exchange_content", pin=False)
        # An unmodified record whose pin is refreshed still reaches the semantics.
        self.reject(lambda r, s: response(s)["message"].update(content="changed"),
                    "feed_exchange_message")

    def test_borrowed_or_unknown_operation_refuses(self):
        for method in ("interaction_ack", "interaction_edit", "db_cas", "db_insert", "source_get"):
            with self.subTest(method=method):
                self.reject(lambda r, s: exchange(s).update(
                    operation_id=operation_of(r, method)["operation_id"]),
                    "feed_exchange_operation")
        self.reject(lambda r, s: exchange(s).update(operation_id="no-such-operation"),
                    "feed_exchange_operation")


class Route(FeedExchange):
    def test_route_must_be_the_fixed_channel_message_post(self):
        for path in ("/channels/501/messages", "/channels/999/messages", "/channels/1/messages"):
            with self.subTest(path=path):
                self.reject(lambda r, s: exchange(s).update(path=path), "feed_exchange_route")

    def test_unrepresentable_routes_fail_closed_at_intake(self):
        for field, value in (("origin", "http://discord.com"), ("origin", "https://discord.com:443"),
                             ("origin", "https://discordapp.com"), ("origin", "https://discord.com/"),
                             ("http_method", "PUT"), ("http_method", "GET"),
                             ("http_method", "post"), ("http_method", "PATCH"),
                             ("path", "/channels/500/messages/"),
                             ("path", "/channels/0500/messages"),
                             ("path", "/channels/500/messages?wait=true"),
                             ("path", "/channels/500/messages/9201"),
                             ("path", "/channels/500/threads"),
                             ("path", "//channels/500/messages"),
                             ("path", "/api/v10/channels/500/messages"),
                             ("path", "/webhooks/500/token")):
            with self.subTest(field=field, value=value):
                self.reject(lambda r, s: exchange(s).update({field: value}))


class Body(FeedExchange):
    def test_malformed_and_non_object_bodies_refuse(self):
        for text, code in (("{", "json"), ("NaN", "non_json_number"), ("{'a':1}", "json"),
                           (raw({"a": 1}) + "x", "json"), ("[]", "object_fields"),
                           ("5", "object_fields"), ('"s"', "object_fields"),
                           ("true", "object_fields")):
            with self.subTest(text=text):
                self.reject(with_body(text), code)
        self.reject(with_body(""))

    def test_duplicate_keys_refuse_even_when_the_last_value_is_correct(self):
        good = raw(body(feed_exchange_fixture()[1]))
        nonce = json.loads(good)["nonce"]
        nested = good.replace('"parse":[]', '"parse":["users"],"parse":[]')
        for text in (good[:-1] + ',"nonce":"' + nonce + '"}',
                     '{"content":"x",' + good[1:], nested):
            with self.subTest(text=text[:40]):
                self.reject(with_body(text), "duplicate_key")

    def test_unknown_missing_or_extra_body_fields_refuse(self):
        for key, value in (("tts", False), ("components", []), ("embeds", []),
                           ("message_reference", {}), ("flags", 0), ("sticker_ids", []),
                           ("attachments", []), ("poll", {}), ("username", "x"),
                           ("Content", "x")):
            with self.subTest(add=key):
                self.reject(with_fields(lambda v: v.update({key: value})), "object_fields")
        for key in ("content", "nonce", "enforce_nonce", "allowed_mentions"):
            with self.subTest(drop=key):
                self.reject(with_fields(lambda v: v.pop(key)), "object_fields")
        for number, change in enumerate((lambda m: m.update(extra=[]),
                lambda m: m.pop("replied_user"), lambda m: m.pop("parse"),
                lambda m: m.pop("users"), lambda m: m.pop("roles"), lambda m: m.clear())):
            with self.subTest(mentions=number):
                self.reject(with_fields(lambda v: change(v["allowed_mentions"])), "object_fields")

    def test_types_distinguish_bool_int_string_and_null(self):
        for key, value in (("enforce_nonce", 1), ("enforce_nonce", "true"),
                           ("enforce_nonce", False), ("enforce_nonce", None),
                           ("content", 5), ("content", None), ("nonce", 5), ("nonce", None),
                           ("allowed_mentions", [])):
            with self.subTest(key=key, value=value):
                self.reject(with_fields(lambda v: v.update({key: value})),
                            "object_fields" if key == "allowed_mentions" else "enum")
        for value in (0, "false", None, 0.0):
            with self.subTest(replied_user=value):
                self.reject(with_fields(lambda v: v["allowed_mentions"].update(replied_user=value)))
        for key, value in (("parse", {}), ("parse", ""), ("users", None), ("roles", 0)):
            with self.subTest(mentions=key):
                self.reject(with_fields(lambda v: v["allowed_mentions"].update({key: value})), "enum")

    def test_nonce_and_content_must_match_the_source_selection_exactly(self):
        nonce = body(feed_exchange_fixture()[1])["nonce"]
        url = "https://fixture.example.test/item"
        for value in ("0" * 24, nonce.upper() if nonce.upper() != nonce else "F" * 24,
                      nonce[:-1], nonce + "0", ""):
            with self.subTest(nonce=value):
                self.reject(with_fields(lambda v: v.update(nonce=value)), "enum")
        for value in ("New feed item: Fixture item\n" + url, "New feed item: **Fixture item**",
                      "New feed item: **Fixture item**\n" + url + "\n",
                      "New feed item: **Fixture item**\r\n" + url,
                      "New feed item: **Fixture item**\n" + url + "/x",
                      "@everyone New feed item: **Fixture item**\n" + url, "",
                      "new feed item: **Fixture item**\n" + url):
            with self.subTest(content=value):
                self.reject(with_fields(lambda v: v.update(content=value)), "enum")

    def test_allowed_mentions_must_be_the_complete_empty_policy(self):
        for key, value in (("parse", ["users"]), ("parse", ["roles"]), ("parse", ["everyone"]),
                           ("roles", ["1"]), ("users", ["300"]), ("replied_user", True)):
            with self.subTest(key=key, value=value):
                self.reject(with_fields(lambda v: v["allowed_mentions"].update({key: value})), "enum")

    def test_payload_digest_must_equal_the_actual_body_bytes(self):
        self.reject(lambda r, s: set_payload(r, s, post_operation(r)["operation_id"], "f" * 64),
                    "feed_exchange_payload", payload=False)
        # Semantically equal JSON with other bytes is a different digest.
        self.reject(lambda r, s: exchange(s).update(
            body_utf8=raw(body(s)).replace(",", ", ")), "feed_exchange_payload", payload=False)


class Response(FeedExchange):
    def test_message_receipt_id_must_fit_nonzero_u64(self):
        def rebind(receipt, records, identifier):
            set_platform_id(receipt, records, post_operation(receipt)["operation_id"], identifier)
            message(records)["id"] = identifier
        self.accept(lambda r, s: rebind(r, s, str(2**64 - 1)))
        self.reject(lambda r, s: rebind(r, s, str(2**64)), "feed_exchange_identity")

    def test_projected_channel_and_author_ids_must_fit_nonzero_u64(self):
        for field in ("channel_id", "author_id"):
            with self.subTest(field=field):
                self.reject(lambda r, s: message(s).update({field: str(2**64)}),
                            "feed_exchange_identity")

    def test_status_must_be_exactly_http_200(self):
        for status in (100, 201, 204, 400, 401, 403, 404, 429, 500, 599):
            with self.subTest(status=status):
                self.reject(lambda r, s: response(s).update(http_status=status),
                            "feed_exchange_status")

    def test_message_ids_must_bind_the_accepted_post(self):
        for key, value in (("id", "9999"), ("id", "92010"), ("channel_id", "501"),
                           ("channel_id", "100"), ("author_id", "300"), ("author_id", "999")):
            with self.subTest(key=key, value=value):
                self.reject(lambda r, s: message(s).update({key: value}), "feed_exchange_message")

    def test_platform_identity_change_leaves_a_stale_response_id(self):
        # The ledger/platform claim moves coherently; the response still names the old ID.
        self.reject(lambda r, s: set_platform_id(r, s, post_operation(r)["operation_id"], "9777"),
                    "feed_exchange_message")

    def test_message_content_nonce_flags_and_lists_must_be_exact(self):
        self.reject(lambda r, s: message(s).update(content=""), "text_format")
        for key, value in (("content", "changed"),
                           ("nonce", "0" * 24), ("flags", 64), ("flags", 1), ("flags", 4096),
                           ("mentions", ["300"]), ("mention_roles", ["1"])):
            with self.subTest(key=key, value=value):
                self.reject(lambda r, s: message(s).update({key: value}), "feed_exchange_message")
        self.reject(lambda r, s: message(s).update(content=message(s)["content"] + " "),
                    "feed_exchange_message")

    def test_response_shape_and_closed_values_fail_closed_at_intake(self):
        for number, change in enumerate((lambda s: message(s).update(flags=True),
                       lambda s: message(s).update(flags=0.0),
                       lambda s: message(s).update(flags="0"), lambda s: message(s).update(id=9201),
                       lambda s: message(s).update(mentions=[300]),
                       lambda s: message(s).update(mention_everyone=True),
                       lambda s: message(s).update(mention_everyone=0),
                       lambda s: message(s).update(attachments=[{}]),
                       lambda s: message(s).update(nonce="0" * 23),
                       lambda s: message(s).update(embeds=[]),
                       lambda s: message(s).pop("nonce"),
                       lambda s: response(s).update(http_status=True),
                       lambda s: response(s).update(http_status="200"),
                       lambda s: response(s).update(received_at="2026-01-01T00:08:00+00:00"),
                       lambda s: response(s).update(observer_ref="UPPER"),
                       lambda s: response(s).update(extra=1),
                       lambda s: response(s).pop("message"))):
            with self.subTest(change=number):
                self.reject(lambda r, s: change(s))

    def test_response_time_is_bounded_by_post_platform_and_delivery_cas(self):
        # Platform observation is 00:08:00; the delivery CAS is 00:08:10.
        for clock in ("00:07:59", "00:08:05", "00:08:11"):
            with self.subTest(clock=clock):
                self.reject(lambda r, s: response(s).update(received_at="2026-01-01T" + clock + "Z"),
                            "feed_exchange_observation")
        self.reject(lambda r, s: move_platform(r, s, "00:08:20"), "feed_exchange_observation")
        # At the case end the upstream platform scope refuses first; either way it refuses.
        self.reject(lambda r, s: move_platform(r, s, "00:09:00"))

    def test_response_observer_must_be_the_coverage_observer(self):
        # The coverage observer is "observer-ref"; any other principal differs.
        for observer in ("other-observer", "resource-observer"):
            with self.subTest(observer=observer):
                self.reject(lambda r, s: response(s).update(observer_ref=observer),
                            "feed_exchange_observation")


class Upstream(FeedExchange):
    def test_selected_item_text_drift_refuses_a_stale_exchange(self):
        for changes in ({"title": "Fixture-item"}, {"url": "https://fixture.example.test/other"},
                        {"title": "Other title", "url": "https://fixture.example.test/two"}):
            with self.subTest(changes=changes):
                self.reject(lambda r, s: rebind_item(r, s, **changes), "enum")

    def test_source_hash_nonce_and_content_drift_refuse_before_exchange_semantics(self):
        self.reject(lambda r, s: s["selected-item"]["data"].update(nonce="a" * 24), "source_nonce")
        self.reject(lambda r, s: s["selected-item"]["data"].update(post_content_sha256="a" * 64),
                    "source_post")
        self.reject(lambda r, s: s["selected-item"]["data"].update(body_sha256="f" * 64),
                    "source_selection")
        self.reject(lambda r, s: s["source-body"]["data"].update(sha256="a" * 64), "source_body")

    def test_unknown_effect_outcome_cannot_become_a_posted_message(self):
        def unknown(receipt, records):
            for ledger in ("actual_effects", "expected_effects"):
                for effect in receipt["observations"][ledger]:
                    if effect["method"] == "feed_post":
                        effect["outcome"] = "unknown"
        # Upstream reconciliation/telemetry refuses; no exchange can repair it.
        self.reject(unknown)


if __name__ == "__main__":
    unittest.main()
