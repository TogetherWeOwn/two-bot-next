"""Hermetic interaction-exchange adversaries; not Discord or stored-data proof.

GitHub Actions is the suite's CI of record; focused reproductions are disclosed
separately. Passing shows only that fabricated request/response projections agree
with a fabricated effect ledger. Nothing is observed or authenticated and
runtime_preflight stays unsupported. Most negatives repin the other derived
claims (body, request, telemetry, input chain) so the interaction checker, not
a stale upstream hash, is what refuses. Where an intake schema refuses first the
test asserts only a Refusal; exact codes are asserted only where predictable.
"""

import copy
from datetime import datetime, timedelta
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import announcement_interaction_fixtures as fixtures
from announcement_interaction_fixtures import (
    DISABLED, EXCHANGES, PLAN, SAVED, interaction_fixture, repin_interactions, set_payload,
    set_platform_id)
from announcement_provenance_fixtures import evidence_index
from announcement_runtime_interactions import check_interaction_exchange_claims
from announcement_runtime_policy import runtime_preflight
from announcement_runtime_schema import Refusal

STAMP = "%Y-%m-%dT%H:%M:%SZ"


def exchanges(records):
    return records[EXCHANGES]["data"]["exchanges"]


def exchange_of(records, case_id):
    return next(x for x in exchanges(records) if x["case_id"] == case_id)


def response_of(records, case_id, route):
    return next(x for x in exchange_of(records, case_id)["responses"] if x["route"] == route)


def plan_of(records, case_id):
    return next(x for x in records[PLAN]["data"]["interactions"] if x["case_id"] == case_id)


def case_of(receipt, case_id):
    return next(x for x in receipt["observations"]["cases"] if x["case_id"] == case_id)


def shifted(stamp, seconds):
    return (datetime.strptime(stamp, STAMP) + timedelta(seconds=seconds)).strftime(STAMP)


def with_body(body):
    def mutate(receipt, records):
        response_of(records, "on-rsvp", "interaction_callback")["body_utf8"] = body
    return mutate


class Interactions(unittest.TestCase):
    def check(self, receipt, records, now):
        return check_interaction_exchange_claims(receipt, evidence_index(records), now)

    def reject(self, mutate, code=None, **repin):
        receipt, records, now = interaction_fixture()
        mutate(receipt, records)
        repin_interactions(receipt, records, **repin)
        with self.assertRaises(Refusal) as caught:
            self.check(receipt, records, now)
        if code is not None:
            self.assertEqual(code, caught.exception.code)

    def accept(self, mutate, **repin):
        receipt, records, now = interaction_fixture()
        mutate(receipt, records)
        repin_interactions(receipt, records, **repin)
        self.assertIsNone(self.check(receipt, records, now))

    def refuse_intake(self, mutate):
        _, records, _ = interaction_fixture()
        mutate(records)
        with self.assertRaises(Refusal):
            evidence_index(records)


class Coherence(Interactions):
    def test_baseline_is_bounded_returns_none_and_is_not_runtime_authority(self):
        receipt, records, now = interaction_fixture()
        self.assertLessEqual(len(records), 256)
        for name in ("cases", "actual_effects", "expected_effects"):
            self.assertLessEqual(len(receipt["observations"][name]), 256)
        index = evidence_index(records)
        self.assertIsNone(check_interaction_exchange_claims(receipt, index, now))
        # Inherited authority sidecars stay unconsumed: complete packets still refuse.
        with self.assertRaisesRegex(Refusal, "unreferenced_evidence$"):
            index.require_all_used()
        self.assertFalse(receipt["promotable"])
        with self.assertRaisesRegex(Refusal, "unsupported"):
            runtime_preflight()

    def test_single_aggregate_record_covers_eight_off_commands_and_one_rsvp(self):
        _, records, _ = interaction_fixture()
        self.assertEqual(1, sum(x["kind"] == "interaction_exchanges" for x in records.values()))
        items = exchanges(records)
        self.assertEqual(9, len(items))
        off = [x for x in items if x["case_id"].startswith("off-")]
        self.assertEqual(8, len({x["request"]["command"] for x in off}))
        for item in off:
            self.assertEqual([DISABLED], [x["readback"]["content"] for x in item["responses"]])
            self.assertEqual([], item["request"]["options"])

    def test_positive_rsvp_acks_then_edits_the_same_original_message(self):
        _, records, _ = interaction_fixture()
        ack = response_of(records, "on-rsvp", "interaction_callback")
        edit = response_of(records, "on-rsvp", "original_response")
        self.assertEqual(ack["readback"]["message_id"], edit["readback"]["message_id"])
        self.assertEqual("", ack["readback"]["content"])
        self.assertEqual(SAVED, edit["readback"]["content"])
        self.assertEqual("RSVP saved: going.", SAVED)

    def test_plan_pins_exchange_digest_and_request_hashes(self):
        _, records, _ = interaction_fixture()
        self.assertEqual(EXCHANGES, records[PLAN]["data"]["exchanges"]["ref"])
        self.assertEqual(64, len(records[PLAN]["data"]["exchanges"]["sha256"]))
        self.assertNotEqual("a" * 64, plan_of(records, "on-rsvp")["request_sha256"])

    def test_fixture_is_synthetic_and_calls_do_not_share_state(self):
        self.assertIn("SYNTHETIC", fixtures.__doc__)
        first = interaction_fixture()
        first[1][EXCHANGES]["data"]["exchanges"].clear()
        self.assertEqual(9, len(exchanges(interaction_fixture()[1])))
        self.assertEqual(copy.deepcopy(interaction_fixture()[1]), interaction_fixture()[1])

    def test_omitted_empty_mention_fields_are_accepted_when_rehashed(self):
        self.accept(with_body('{"type":5,"data":{"flags":64,"allowed_mentions":{"parse":[]}}}'))

    def test_whitespace_changes_follow_the_raw_bytes_when_rehashed(self):
        self.accept(with_body(fixtures.ack_body(True).replace(",", ", ")))


class RawBodies(Interactions):
    def test_malformed_bodies_refuse(self):
        for text, code in (("{", "json"), ("NaN", "non_json_number"), ("{'a':1}", "json"),
                           ('{"type":5,"data":{"flags":64,"allowed_mentions":{"parse":[]}}}x', "json")):
            with self.subTest(text=text):
                self.reject(with_body(text), code)

    def test_non_object_bodies_refuse(self):
        for text in ("[]", "5", '"s"', "true"):
            with self.subTest(text=text):
                self.reject(with_body(text), "interaction_body")

    def test_duplicate_keys_refuse(self):
        body = fixtures.ack_body(True)
        for text in (body.replace('"flags":64', '"flags":64,"flags":64'),
                     body.replace('"type":5', '"type":5,"type":5'),
                     body.replace('"parse":[]', '"parse":[],"parse":[]')):
            with self.subTest(text=text):
                self.reject(with_body(text), "duplicate_key")

    def test_unknown_and_extra_fields_refuse(self):
        body = fixtures.ack_body(True)
        self.reject(with_body(body.replace('{"type":5', '{"extra":1,"type":5')), "object_fields")
        self.reject(with_body(body.replace('"flags":64', '"flags":64,"content":"x"')), "object_fields")
        self.reject(with_body(body.replace('"parse":[]', '"parse":[],"everyone":[]')),
                    "interaction_mentions")
        self.reject(lambda r, s: response_of(s, "on-rsvp", "original_response").update(
            body_utf8=fixtures.edit_body().replace('{"content"', '{"flags":64,"content"')),
            "object_fields")

    def test_missing_or_enabled_mentions_refuse(self):
        body = fixtures.ack_body(True)
        self.reject(with_body(body.replace(',"allowed_mentions":' + fixtures.raw(fixtures.MENTIONS), "")),
                    "interaction_mentions")
        self.reject(with_body(body.replace('"allowed_mentions":' + fixtures.raw(fixtures.MENTIONS),
                                           '"allowed_mentions":null')), "interaction_mentions")
        self.reject(with_body(body.replace('"parse":[],', "")), "interaction_mentions")
        for old, new in (('"parse":[]', '"parse":["users"]'), ('"users":[]', '"users":["300"]'),
                         ('"roles":[]', '"roles":["1"]'), ('"replied_user":false', '"replied_user":true')):
            with self.subTest(new=new):
                self.reject(with_body(body.replace(old, new)), "enum")

    def test_wrong_ack_shapes_refuse(self):
        positive = fixtures.ack_body(True)
        self.reject(with_body(positive.replace('"type":5', '"type":4')), "interaction_ack_body")
        self.reject(with_body(positive.replace('"type":5', '"type":6')), "integer")
        self.reject(with_body(positive.replace('"flags":64', '"flags":0')), "interaction_ack_body")
        self.reject(with_body(positive.replace('"flags":64', '"flags":"64"')), "integer")
        self.reject(with_body(positive.replace('"flags":64', '"flags":true')), "integer")
        self.reject(with_body(fixtures.ack_body(False)), "object_fields")

    def test_wrong_disabled_ack_refuses(self):
        def off(mutate):
            def run(receipt, records):
                response_of(records, "off-feed-add", "interaction_callback")["body_utf8"] = mutate(
                    fixtures.ack_body(False))
            return run
        self.reject(off(lambda b: b.replace("disabled", "Disabled")), "interaction_ack_body")
        self.reject(off(lambda b: b.replace('"type":4', '"type":5')), "interaction_ack_body")
        self.reject(off(lambda b: b.replace('"flags":64', '"flags":0')), "interaction_ack_body")
        self.reject(off(lambda b: b.replace(' this is a host setting, not a Discord role.', "")),
                    "interaction_ack_body")
        self.reject(off(lambda b: b.replace('"content":' + fixtures.raw(DISABLED) + ",", "")),
                    "object_fields")
        self.reject(off(lambda b: fixtures.ack_body(True)), "object_fields")

    def test_wrong_edit_text_refuses(self):
        for text in ("RSVP saved: interested.", "RSVP saved: going", "rsvp saved: going.", " RSVP saved: going."):
            with self.subTest(text=text):
                self.reject(lambda r, s, t=text: response_of(s, "on-rsvp", "original_response").update(
                    body_utf8=fixtures.raw({"content": t, "allowed_mentions": fixtures.MENTIONS})),
                    "interaction_edit_body")

    def test_each_of_the_eight_off_commands_requires_exact_text(self):
        _, records, _ = interaction_fixture()
        for item in [x for x in exchanges(records) if x["case_id"].startswith("off-")]:
            with self.subTest(case=item["case_id"]):
                self.reject(lambda r, s, c=item["case_id"]: response_of(
                    s, c, "interaction_callback").update(body_utf8=fixtures.ack_body(False).replace(
                        "Announcements", "Announcement")), "interaction_ack_body")


class BodyDigests(Interactions):
    def test_payload_digest_must_match_raw_body(self):
        def stale(receipt, records):
            for operation in [x["operation_id"] for x in exchange_of(records, "on-rsvp")["responses"]]:
                set_payload(receipt, records, operation, "b" * 64)
        self.reject(stale, "interaction_body_digest", bodies=False)

    def test_body_change_without_rehash_refuses(self):
        self.reject(with_body(fixtures.ack_body(True).replace(",", ", ")),
                    "interaction_body_digest", bodies=False)

    def test_each_response_digest_is_checked_independently(self):
        def stale(receipt, records):
            operation = response_of(records, "on-rsvp", "original_response")["operation_id"]
            set_payload(receipt, records, operation, "c" * 64)
        self.reject(stale, "interaction_body_digest", bodies=False)

    def test_ledger_payload_without_platform_copy_refuses_upstream(self):
        def split(receipt, records):
            operation = response_of(records, "on-rsvp", "interaction_callback")["operation_id"]
            for effect in receipt["observations"]["actual_effects"]:
                if effect["operation_id"] == operation:
                    effect["payload_sha256"] = "d" * 64
        self.reject(split, None, bodies=False)


class ObserverAndTime(Interactions):
    def test_expired_initial_ack_refuses_even_inside_case_window(self):
        self.reject(lambda r, s: exchange_of(s, "on-rsvp").update(
            received_at=shifted(exchange_of(s, "on-rsvp")["received_at"], -10)),
            "interaction_ack_deadline")

    def test_coarse_initial_ack_deadline_boundary(self):
        self.accept(lambda r, s: exchange_of(s, "on-rsvp").update(
            received_at=shifted(exchange_of(s, "on-rsvp")["received_at"], -2)))
        for case_id in ("on-rsvp", "off-feed-add"):
            with self.subTest(case_id=case_id):
                self.reject(lambda r, s, c=case_id: exchange_of(s, c).update(
                    received_at=shifted(exchange_of(s, c)["received_at"], -3)),
                    "interaction_ack_deadline")

    def test_observer_must_match_coverage(self):
        self.reject(lambda r, s: exchange_of(s, "on-rsvp").update(observer_ref="other-observer"),
                    "interaction_received")

    def test_receipt_must_be_inside_the_case_window(self):
        self.reject(lambda r, s: exchange_of(s, "on-rsvp").update(
            received_at=shifted(case_of(r, "on-rsvp")["started_at"], -1)), "interaction_received")
        self.reject(lambda r, s: exchange_of(s, "on-rsvp").update(
            received_at=case_of(r, "on-rsvp")["ended_at"]), "interaction_received")
        self.reject(lambda r, s: exchange_of(s, "on-rsvp").update(
            received_at=shifted(case_of(r, "on-rsvp")["ended_at"], 1)), "interaction_received")

    def test_receipt_may_not_follow_a_response(self):
        def late(receipt, records):
            item = exchange_of(records, "on-rsvp")
            item["received_at"] = shifted(item["received_at"], 1)
        self.reject(late, "interaction_response_order")

    def test_bad_timestamp_shapes_refuse_at_intake(self):
        for value in ("2026-01-01T00:07:30", "2026-01-01 00:07:30Z", "2026-13-01T00:07:30Z", 5):
            with self.subTest(value=value):
                self.refuse_intake(lambda s, v=value: exchange_of(s, "on-rsvp").update(received_at=v))


class Requests(Interactions):
    def test_request_identity_fields_must_match_claims(self):
        for field, value in (("interaction_id", "7"), ("application_id", "7"), ("guild_id", "7"),
                             ("channel_id", "7"), ("actor_id", "7"), ("command", "feed-list")):
            with self.subTest(field=field):
                self.reject(lambda r, s, f=field, v=value: exchange_of(s, "on-rsvp")["request"].update(
                    {f: v}), "interaction_request")

    def test_option_claims_are_exact(self):
        def options(change):
            return lambda r, s: change(exchange_of(s, "on-rsvp")["request"])
        self.reject(options(lambda q: q.update(options=[])), "interaction_options")
        self.reject(options(lambda q: q["options"].pop()), "interaction_options")
        self.reject(options(lambda q: q["options"][0].update(value="7")), "interaction_options")
        self.reject(options(lambda q: q["options"][1].update(value="interested")), "interaction_options")
        self.reject(options(lambda q: q["options"].append(copy.deepcopy(q["options"][0]))),
                    "array_size")
        def duplicate(request):
            request["options"][1] = copy.deepcopy(request["options"][0])
        self.reject(options(duplicate), "interaction_options")
        self.reject(lambda r, s: exchange_of(s, "off-feed-add")["request"].update(
            options=[{"name": "status", "value": "going"}]), "interaction_options")

    def test_unique_options_may_arrive_in_either_order(self):
        self.accept(lambda r, s: exchange_of(s, "on-rsvp")["request"]["options"].reverse())

    def test_unknown_option_shapes_refuse_at_intake(self):
        for option in ({"name": "other", "value": "1"}, {"name": "status"},
                       {"name": "status", "value": "going", "extra": 1}, {"name": "status", "value": "maybe"}):
            with self.subTest(option=option):
                self.refuse_intake(lambda s, o=option: exchange_of(s, "on-rsvp")["request"].update(options=[o]))

    def test_request_hash_must_match_the_plan(self):
        self.reject(lambda r, s: plan_of(s, "on-rsvp").update(request_sha256="e" * 64),
                    "interaction_request", requests=False)
        self.reject(lambda r, s: exchange_of(s, "off-feed-add")["request"].update(actor_id="7"),
                    "interaction_request", requests=False)

    def test_request_extra_or_missing_fields_refuse_at_intake(self):
        self.refuse_intake(lambda s: exchange_of(s, "on-rsvp")["request"].update(token="x"))
        self.refuse_intake(lambda s: exchange_of(s, "on-rsvp")["request"].pop("guild_id"))
        self.refuse_intake(lambda s: exchange_of(s, "on-rsvp")["request"].update(command="nope"))


class Inventory(Interactions):
    def test_missing_exchange_refuses(self):
        self.reject(lambda r, s: exchanges(s).pop(), "interaction_exchange_inventory")
        self.reject(lambda r, s: exchanges(s).pop(0), "interaction_exchange_inventory")

    def test_extra_or_duplicate_exchange_refuses(self):
        def extra(case_id):
            def run(receipt, records):
                item = copy.deepcopy(exchanges(records)[0])
                item["case_id"] = case_id
                exchanges(records).append(item)
            return run
        self.reject(extra("feed-on"), "interaction_exchange_inventory")
        self.reject(extra("unknown-case"), "interaction_exchange_inventory")
        self.reject(lambda r, s: exchanges(s).append(copy.deepcopy(exchanges(s)[0])),
                    "interaction_exchange_inventory")

    def test_empty_exchange_list_refuses_at_intake(self):
        self.refuse_intake(lambda s: exchanges(s).clear())

    def test_exchange_list_is_bounded_at_intake(self):
        def grow(records):
            item = exchanges(records)[0]
            exchanges(records).extend(copy.deepcopy(item) for _ in range(257))
        self.refuse_intake(grow)

    def test_missing_extra_or_foreign_response_operation_refuses(self):
        self.reject(lambda r, s: exchange_of(s, "on-rsvp")["responses"].pop(),
                    "interaction_response_inventory")
        self.reject(lambda r, s: exchange_of(s, "on-rsvp")["responses"].pop(0),
                    "interaction_response_inventory")
        self.reject(lambda r, s: exchange_of(s, "off-feed-add")["responses"].append(
            copy.deepcopy(response_of(s, "on-rsvp", "original_response"))),
            "interaction_response_inventory")
        self.reject(lambda r, s: response_of(s, "on-rsvp", "original_response").update(
            operation_id="unknown-operation"), "interaction_response_inventory")

    def test_duplicate_response_operation_refuses(self):
        def duplicate(receipt, records):
            responses = exchange_of(records, "on-rsvp")["responses"]
            responses[1] = copy.deepcopy(responses[0])
        self.reject(duplicate, "reference_duplicate")

    def test_response_list_bounds_refuse_at_intake(self):
        self.refuse_intake(lambda s: exchange_of(s, "on-rsvp").update(responses=[]))
        self.refuse_intake(lambda s: exchange_of(s, "on-rsvp")["responses"].append(
            copy.deepcopy(exchange_of(s, "on-rsvp")["responses"][0])))

    def test_operation_borrowed_by_another_exchange_refuses(self):
        def borrow(receipt, records):
            donor = response_of(records, "off-feed-add", "interaction_callback")["operation_id"]
            response_of(records, "off-feed-list", "interaction_callback")["operation_id"] = donor
        self.reject(borrow, "interaction_response_inventory")


class Routes(Interactions):
    def test_route_method_and_origin_are_exact(self):
        self.reject(lambda r, s: response_of(s, "on-rsvp", "interaction_callback").update(
            route="original_response"), "interaction_route")
        self.reject(lambda r, s: response_of(s, "on-rsvp", "original_response").update(
            route="interaction_callback"), "interaction_route")
        self.reject(lambda r, s: response_of(s, "on-rsvp", "interaction_callback").update(
            http_method="PATCH"), "interaction_route")
        self.reject(lambda r, s: response_of(s, "on-rsvp", "original_response").update(
            http_method="POST"), "interaction_route")

    def test_unknown_route_method_or_origin_refuse_at_intake(self):
        for field, value in (("origin", "https://example.com"), ("origin", "http://discord.com"),
                             ("route", "followup"), ("http_method", "PUT"), ("http_method", "post")):
            with self.subTest(field=field, value=value):
                self.refuse_intake(lambda s, f=field, v=value: response_of(
                    s, "on-rsvp", "interaction_callback").update({f: v}))

    def test_response_app_and_interaction_must_match_request(self):
        for field in ("interaction_id", "application_id"):
            with self.subTest(field=field):
                self.reject(lambda r, s, f=field: response_of(s, "on-rsvp", "original_response").update(
                    {f: "7"}), "interaction_route")


class Readbacks(Interactions):
    def test_deferred_ack_readback_includes_loading_but_body_does_not(self):
        self.accept(lambda r, s: response_of(s, "on-rsvp", "interaction_callback")["readback"].update(
            flags=192))
        _, records, _ = interaction_fixture()
        self.assertIn('"flags":64', response_of(records, "on-rsvp", "interaction_callback")["body_utf8"])

    def test_loading_is_required_only_for_the_deferred_ack(self):
        self.reject(lambda r, s: response_of(s, "on-rsvp", "interaction_callback")["readback"].update(
            flags=64), "interaction_readback")
        for case_id, route in (("on-rsvp", "original_response"),
                               ("off-feed-add", "interaction_callback")):
            with self.subTest(case_id=case_id, route=route):
                self.reject(lambda r, s, c=case_id, t=route: response_of(s, c, t)["readback"].update(
                    flags=192), "interaction_readback")

    def test_readback_identity_and_ephemeral_flag_are_exact(self):
        for field, value in (("application_id", "7"), ("channel_id", "7"), ("flags", 0),
                             ("flags", 32), ("flags", 128)):
            with self.subTest(field=field, value=value):
                self.reject(lambda r, s, f=field, v=value: response_of(
                    s, "off-feed-add", "interaction_callback")["readback"].update({f: v}),
                    "interaction_readback")

    def test_readback_flag_types_refuse(self):
        for value in (True, "64", None, 64.0):
            with self.subTest(value=value):
                self.refuse_intake(lambda s, v=value: response_of(
                    s, "off-feed-add", "interaction_callback")["readback"].update(flags=v))

    def test_readback_content_is_exact_for_every_phase(self):
        self.reject(lambda r, s: response_of(s, "off-feed-add", "interaction_callback")["readback"].update(
            content=""), "interaction_readback")
        self.reject(lambda r, s: response_of(s, "off-feed-add", "interaction_callback")["readback"].update(
            content=DISABLED + " "), "interaction_readback")
        self.reject(lambda r, s: response_of(s, "on-rsvp", "interaction_callback")["readback"].update(
            content=SAVED), "interaction_readback")
        self.reject(lambda r, s: response_of(s, "on-rsvp", "interaction_callback")["readback"].update(
            content="x"), "interaction_readback")
        self.reject(lambda r, s: response_of(s, "on-rsvp", "original_response")["readback"].update(
            content=""), "interaction_readback")
        self.reject(lambda r, s: response_of(s, "on-rsvp", "original_response")["readback"].update(
            content="RSVP saved: declined."), "interaction_readback")

    def test_readback_message_id_must_match_platform_receipt(self):
        self.reject(lambda r, s: response_of(s, "off-feed-add", "interaction_callback")["readback"].update(
            message_id="7"), "interaction_readback")
        self.reject(lambda r, s: response_of(s, "on-rsvp", "original_response")["readback"].update(
            message_id="7"), "interaction_readback")

    def test_edit_must_target_the_same_original_message_as_the_ack(self):
        def split(receipt, records):
            edit = response_of(records, "on-rsvp", "original_response")
            set_platform_id(receipt, records, edit["operation_id"], "9777")
            edit["readback"]["message_id"] = "9777"
        self.reject(split, "interaction_original_message")

    def test_readback_extra_or_missing_fields_refuse_at_intake(self):
        self.refuse_intake(lambda s: response_of(s, "on-rsvp", "original_response")["readback"].update(
            embeds=[]))
        self.refuse_intake(lambda s: response_of(s, "on-rsvp", "original_response")["readback"].pop(
            "content"))
        self.refuse_intake(lambda s: response_of(s, "on-rsvp", "original_response")["readback"].update(
            message_id="abc"))
        self.refuse_intake(lambda s: response_of(s, "on-rsvp", "original_response")["readback"].update(
            content=None))


class UnsupportedProfiles(Interactions):
    def test_replay_and_negative_profiles_refuse_before_any_exchange_acceptance(self):
        for phase in ("replay", "negative"):
            with self.subTest(phase=phase):
                # Upstream input/scheduler checks pin plan phase to the case phase
                # and refuse replay/negative first, so the exact code is unasserted.
                self.reject(lambda r, s, p=phase: plan_of(s, "off-feed-add").update(phase=p))

    def test_phase_outside_plan_vocabulary_refuses_at_intake(self):
        for phase in ("baseline", "cleanup", "restoration", "ON"):
            with self.subTest(phase=phase):
                self.refuse_intake(lambda s, p=phase: plan_of(s, "off-feed-add").update(phase=p))

    def test_non_rsvp_positive_profile_is_not_a_supported_request(self):
        def promote(receipt, records):
            plan_of(records, "off-feed-add").update(phase="on")
        self.reject(promote)


class Schema(Interactions):
    def test_aggregate_record_is_closed(self):
        self.refuse_intake(lambda s: s[EXCHANGES]["data"].update(extra=[]))
        self.refuse_intake(lambda s: s[EXCHANGES]["data"].pop("exchanges"))
        self.refuse_intake(lambda s: exchange_of(s, "on-rsvp").update(extra=1))
        self.refuse_intake(lambda s: exchange_of(s, "on-rsvp").pop("observer_ref"))
        self.refuse_intake(lambda s: response_of(
            s, "on-rsvp", "original_response").update(extra=1))
        self.refuse_intake(lambda s: response_of(
            s, "on-rsvp", "original_response").pop("body_utf8"))
        self.refuse_intake(lambda s: s[EXCHANGES]["data"].update(exchanges={}))

    def test_scalar_types_and_formats_refuse(self):
        for field, value in (("case_id", "Bad Case"), ("case_id", 1), ("observer_ref", ""),
                             ("received_at", None)):
            with self.subTest(field=field):
                self.refuse_intake(lambda s, f=field, v=value: exchange_of(
                    s, "on-rsvp").update({f: v}))
        for field, value in (("interaction_id", "0"), ("interaction_id", 5), ("guild_id", "01"),
                             ("channel_id", "1" * 21), ("actor_id", "-1")):
            with self.subTest(field=field):
                self.refuse_intake(lambda s, f=field, v=value: exchange_of(
                    s, "on-rsvp")["request"].update({f: v}))

    def test_body_text_is_nonempty_string_within_bounds(self):
        for value in ("", None, 5, ["{}"], "x" * 5000):
            with self.subTest(value=type(value).__name__ + str(len(str(value)))):
                self.refuse_intake(lambda s, v=value: response_of(
                    s, "on-rsvp", "original_response").update(body_utf8=v))

    def test_record_must_belong_to_the_exact_run_attempt(self):
        for field, value in (("attempt", 2), ("run_id", "9"), ("control_sha", "0" * 40)):
            with self.subTest(field=field):
                _, records, _ = interaction_fixture()
                records[EXCHANGES]["run"][field] = value
                with self.assertRaises(Refusal) as caught:
                    evidence_index(records)
                self.assertEqual("evidence_attempt", caught.exception.code)

    def test_record_kind_and_pin_resolution_are_exact(self):
        receipt, records, now = interaction_fixture()
        records[PLAN]["data"]["exchanges"]["ref"] = "runtime-input-plan"
        repin_interactions(receipt, records, pin=False)
        with self.assertRaises(Refusal) as caught:
            self.check(receipt, records, now)
        self.assertEqual("reference_kind", caught.exception.code)
        receipt, records, now = interaction_fixture()
        records[PLAN]["data"]["exchanges"]["ref"] = "no-such-exchanges"
        repin_interactions(receipt, records, pin=False)
        with self.assertRaises(Refusal) as caught:
            self.check(receipt, records, now)
        self.assertEqual("unresolved_reference", caught.exception.code)

    def test_pin_digest_must_match_exchange_content(self):
        def edit(receipt, records):
            exchanges(records).reverse()  # same claims, different canonical bytes
        self.reject(edit, "interaction_exchange_content", pin=False)

    def test_unknown_record_kind_for_old_fixture_pin_is_not_silently_consumed(self):
        from announcement_input_fixtures import input_fixture
        receipt, records, now = input_fixture()
        self.assertNotIn(EXCHANGES, records)
        with self.assertRaises(Refusal):
            self.check(receipt, records, now)


if __name__ == "__main__":
    unittest.main()
