"""Hermetic eligibility-read adversaries; not Discord, transport or database proof.

GitHub Actions is the suite's CI of record. Every test drives the real public
check_interaction_exchange_claims over a real EvidenceIndex built from the current
interaction_fixture(); no upstream checker is mocked. The eligibility_reads are
unauthenticated, hand-written restricted projections of the member and
scheduled-event GETs. Passing shows only that fabricated minimal closed JSON agrees
with a fabricated effect ledger; it does not observe Discord, authenticate any
response, or prove stored RSVP rows.

The closed minimal raw profile is deliberate: a member body is exactly
{"user": {"id"}} and an event body exactly {"id", "guild_id", "status"}. Extra
profile or event fields refuse on purpose. Negatives repin only the derived claims
(repin_interactions, which re-derives telemetry) so the eligibility checker, not a
stale upstream hash, refuses. Tests that depend on an intake schema or on an
earlier checker assert only a Refusal, or that the code is not an eligibility_ one,
so an earlier refusal is never mistaken for a targeted regression.
"""

import copy
import hashlib
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from announcement_case_fixtures import resequence
from announcement_interaction_fixtures import (
    EXCHANGES, PLAN, interaction_fixture, raw, repin_interactions, set_payload)
from announcement_provenance_fixtures import at, evidence_index
from announcement_runtime_interactions import check_interaction_exchange_claims
from announcement_runtime_schema import Refusal

CASE = "on-rsvp"
READ_METHODS = ("member_get", "event_get")
EXPECTED_ORDER = ["member_get", "event_get", "member_get", "event_get"]
# Indexes into the sequence-ordered reads of the positive case.
MEMBER_PRE, EVENT_PRE, MEMBER_POST, EVENT_POST = range(4)


def exchange_of(records, case_id):
    return next(x for x in records[EXCHANGES]["data"]["exchanges"] if x["case_id"] == case_id)


def case_of(receipt, case_id):
    return next(x for x in receipt["observations"]["cases"] if x["case_id"] == case_id)


def effects_of(receipt):
    return {x["operation_id"]: x for x in receipt["observations"]["actual_effects"]}


def ordered_ops(receipt, case_id=CASE):
    """Operation ids of the case's member/event GETs in ledger sequence order."""
    effects = effects_of(receipt)
    refs = [ref for ref in case_of(receipt, case_id)["actual_effect_refs"]
            if effects[ref]["method"] in READ_METHODS]
    return sorted(refs, key=lambda ref: effects[ref]["sequence"])


def read_of(records, operation):
    for exchange in records[EXCHANGES]["data"]["exchanges"]:
        for read in exchange["eligibility_reads"]:
            if read["operation_id"] == operation:
                return read
    raise KeyError(operation)


def reads(records, case_id=CASE):
    return exchange_of(records, case_id)["eligibility_reads"]


def edit(index, **fields):
    def mutate(receipt, records):
        read_of(records, ordered_ops(receipt)[index]).update(fields)
    return mutate


def body(index, value):
    """Replace one read's raw JSON text with the serialization of value."""
    return edit(index, body_utf8=value if isinstance(value, str) else raw(value))


def parsed(records, receipt, index):
    import json
    return json.loads(read_of(records, ordered_ops(receipt)[index])["body_utf8"])


def ids(receipt, records):
    """(guild, actor, event) taken from the fixture's own request projection."""
    request = exchange_of(records, CASE)["request"]
    event = next(x["value"] for x in request["options"] if x["name"] == "event-id")
    return request["guild_id"], request["actor_id"], event


def move(receipt, records, index, clock):
    """Move one read (ledger, wire claim and its received_at) to a new clock time."""
    operation = ordered_ops(receipt)[index]
    effects_of(receipt)[operation]["observed_at"] = at(clock)
    records["wire-" + operation]["data"]["observed_at"] = at(clock)
    read_of(records, operation)["received_at"] = at(clock)
    resequence(receipt, records)


def sha(data):
    return hashlib.sha256(data).hexdigest()


class Eligibility(unittest.TestCase):
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

    def reject_before_eligibility(self, mutate, **repin):
        """Refused, but by an earlier schema or JSON layer, not an eligibility_ check."""
        receipt, records, now = interaction_fixture()
        mutate(receipt, records)
        repin_interactions(receipt, records, **repin)
        with self.assertRaises(Refusal) as caught:
            self.check(receipt, records, now)
        self.assertFalse(caught.exception.code.startswith("eligibility_"), caught.exception.code)

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


class Baseline(Eligibility):
    def test_positive_baseline_has_four_ordered_reads_and_passes(self):
        receipt, records, now = interaction_fixture()
        effects = effects_of(receipt)
        operations = ordered_ops(receipt)
        self.assertEqual(EXPECTED_ORDER, [effects[x]["method"] for x in operations])
        self.assertEqual(set(operations), {x["operation_id"] for x in reads(records)})
        self.assertIsNone(self.check(receipt, records, now))

    def test_baseline_requests_are_empty_exact_gets(self):
        receipt, records, _ = interaction_fixture()
        guild, actor, event = ids(receipt, records)
        effects = effects_of(receipt)
        for operation in ordered_ops(receipt):
            read = read_of(records, operation)
            member = effects[operation]["method"] == "member_get"
            self.assertEqual("https://discord.com", read["origin"])
            self.assertEqual("GET", read["http_method"])
            self.assertEqual(f"/guilds/{guild}/members/{actor}" if member else
                             f"/guilds/{guild}/scheduled-events/{event}", read["path"])
            self.assertEqual(sha(b""), effects[operation]["payload_sha256"])
            self.assertEqual(200, read["http_status"])

    def test_off_cases_carry_no_reads(self):
        _, records, _ = interaction_fixture()
        for exchange in records[EXCHANGES]["data"]["exchanges"]:
            if exchange["case_id"] != CASE:
                with self.subTest(case=exchange["case_id"]):
                    self.assertEqual([], exchange["eligibility_reads"])

    def test_baseline_bodies_are_minimal_closed_json(self):
        receipt, records, _ = interaction_fixture()
        guild, actor, event = ids(receipt, records)
        self.assertEqual({"user": {"id": actor}}, parsed(records, receipt, MEMBER_PRE))
        self.assertEqual({"id": event, "guild_id": guild, "status": 2},
                         parsed(records, receipt, EVENT_PRE))


class EventStatus(Eligibility):
    def event_body(self, index, **changes):
        def mutate(receipt, records):
            value = parsed(records, receipt, index)
            value.update(changes)
            body(index, value)(receipt, records)
        return mutate

    def test_scheduled_active_and_completed_are_allowed_on_every_event_read(self):
        for index in (EVENT_PRE, EVENT_POST):
            for status in (1, 2, 3):
                with self.subTest(index=index, status=status):
                    self.accept(self.event_body(index, status=status))

    def test_out_of_range_event_status_refuses(self):
        for index in (EVENT_PRE, EVENT_POST):
            for status in (0, 4, -1, 2 ** 40):
                with self.subTest(index=index, status=status):
                    self.reject(self.event_body(index, status=status), "integer")

    def test_event_status_must_be_a_json_integer(self):
        for status in (True, False, "2", "going", None, [], {}):
            with self.subTest(status=status):
                self.reject(self.event_body(EVENT_PRE, status=status), "integer")
        # 2.0 may be refused by the number layer or the integer layer; either is a refusal.
        def real(receipt, records):
            read = read_of(records, ordered_ops(receipt)[EVENT_PRE])
            read["body_utf8"] = read["body_utf8"].replace('"status":2', '"status":2.0')
        self.reject_before_eligibility(real)

    def test_event_id_and_guild_must_echo_the_request(self):
        for index in (EVENT_PRE, EVENT_POST):
            for field in ("id", "guild_id"):
                with self.subTest(index=index, field=field):
                    self.reject(self.event_body(index, **{field: "7"}), "eligibility_event")

    def test_event_echo_of_the_actor_or_wrong_guild_refuses(self):
        def swap(receipt, records):
            guild, actor, event = ids(receipt, records)
            body(EVENT_PRE, {"id": actor, "guild_id": guild, "status": 2})(receipt, records)
        self.reject(swap, "eligibility_event")

    def test_malformed_event_ids_refuse_before_the_echo_check(self):
        for field in ("id", "guild_id"):
            for value in ("0", "01", "", -1, 600, None, "1" * 21):
                with self.subTest(field=field, value=value):
                    self.reject_before_eligibility(self.event_body(EVENT_PRE, **{field: value}))


class MemberEcho(Eligibility):
    def test_member_user_id_must_echo_the_actor_on_both_reads(self):
        for index in (MEMBER_PRE, MEMBER_POST):
            with self.subTest(index=index):
                self.reject(body(index, {"user": {"id": "7"}}), "eligibility_member")

    def test_member_echoing_the_event_or_guild_id_refuses(self):
        def echo(which):
            def mutate(receipt, records):
                guild, actor, event = ids(receipt, records)
                body(MEMBER_PRE, {"user": {"id": {"g": guild, "e": event}[which]}})(receipt, records)
            return mutate
        for which in ("g", "e"):
            with self.subTest(which=which):
                self.reject(echo(which), "eligibility_member")

    def test_malformed_member_ids_refuse_before_the_echo_check(self):
        for value in ("0", "01", "", -1, 300, None, "1" * 21):
            with self.subTest(value=value):
                self.reject_before_eligibility(body(MEMBER_PRE, {"user": {"id": value}}))


class HttpStatus(Eligibility):
    def test_non_200_statuses_refuse_on_every_read(self):
        for index in range(4):
            for status in (100, 201, 204, 299, 301, 400, 401, 403, 404, 429, 500, 503, 599):
                with self.subTest(index=index, status=status):
                    self.reject(edit(index, http_status=status), "eligibility_status")

    def test_error_status_with_a_valid_looking_body_still_refuses(self):
        self.reject(edit(MEMBER_PRE, http_status=404), "eligibility_status")
        self.reject(edit(EVENT_POST, http_status=500), "eligibility_status")

    def test_status_range_and_type_refuse_at_intake(self):
        for value in (99, 600, 0, -1, True, False, "200", 200.0, None):
            with self.subTest(value=value):
                self.refuse_intake(lambda s, v=value: reads(s)[0].update(http_status=v))


class Inventory(Eligibility):
    def test_missing_read_record_refuses(self):
        for index in range(4):
            with self.subTest(index=index):
                self.reject(lambda r, s, i=index: reads(s).pop(i), "eligibility_inventory")

    def test_empty_read_list_on_the_positive_case_refuses(self):
        self.reject(lambda r, s: reads(s).clear(), "eligibility_inventory")

    def test_unknown_operation_record_refuses(self):
        self.reject(edit(EVENT_PRE, operation_id="unknown-operation"), "eligibility_inventory")

    def test_duplicate_operation_record_refuses(self):
        def duplicate(receipt, records):
            items = reads(records)
            items[1] = copy.deepcopy(items[0])
        self.reject(duplicate, "eligibility_inventory")

    def test_repeating_one_record_for_two_operations_refuses(self):
        def repeat(receipt, records):
            items = reads(records)
            items[3] = copy.deepcopy(items[2])
        self.reject(repeat, "eligibility_inventory")

    def test_off_case_borrowing_a_positive_read_refuses(self):
        def borrow(receipt, records):
            reads(records, "off-feed-add").append(copy.deepcopy(reads(records)[0]))
        self.reject(borrow, "eligibility_inventory")

    def test_every_off_case_refuses_any_read_record(self):
        _, records, _ = interaction_fixture()
        for exchange in records[EXCHANGES]["data"]["exchanges"]:
            if exchange["case_id"] == CASE:
                continue
            with self.subTest(case=exchange["case_id"]):
                self.reject(lambda r, s, c=exchange["case_id"]: reads(s, c).append(
                    copy.deepcopy(reads(s)[1])), "eligibility_inventory")

    def test_moving_a_read_from_the_positive_case_to_an_off_case_refuses(self):
        def move_record(receipt, records):
            reads(records, "off-feed-add").append(reads(records).pop(0))
        self.reject(move_record, "eligibility_inventory")

    def test_read_list_is_bounded_at_intake(self):
        self.refuse_intake(lambda s: reads(s).append(copy.deepcopy(reads(s)[0])))
        self.refuse_intake(lambda s: reads(s, "off-feed-add").extend(
            copy.deepcopy(reads(s)[0]) for _ in range(5)))


class Requests(Eligibility):
    def test_origin_and_method_are_closed_at_intake(self):
        for field, value in (("origin", "https://example.com"), ("origin", "http://discord.com"),
                             ("origin", "https://discord.com/"), ("http_method", "POST"),
                             ("http_method", "get"), ("http_method", "HEAD")):
            with self.subTest(field=field, value=value):
                self.refuse_intake(lambda s, f=field, v=value: reads(s)[0].update({f: v}))

    def test_path_shapes_outside_the_two_routes_refuse_at_intake(self):
        for path in ("", "/", "/guilds/1/roles/2", "/guilds/1/members/", "/guilds//members/2",
                     "/guilds/1/members/2/", "/guilds/1/members/2?x=1", "/guilds/1/members/2#f",
                     "/guilds/0/members/2", "/guilds/1/members/02", "/guilds/1/members/-2",
                     "/guilds/1/members/2/roles", "guilds/1/members/2", "/guilds/a/members/2",
                     "https://discord.com/guilds/1/members/2", "/guilds/1/scheduled-events/2/users",
                     "/guilds/1/events/2", "/guilds/1/members/2\n", "/GUILDS/1/members/2"):
            with self.subTest(path=path):
                self.refuse_intake(lambda s, p=path: reads(s)[0].update(path=p))

    def test_wrong_guild_actor_or_event_in_the_path_refuses(self):
        for index, field in ((MEMBER_PRE, "guild"), (MEMBER_PRE, "actor"), (MEMBER_POST, "actor"),
                             (EVENT_PRE, "guild"), (EVENT_PRE, "event"), (EVENT_POST, "event")):
            def wrong(receipt, records, i=index, f=field):
                guild, actor, event = ids(receipt, records)
                if f == "guild":
                    guild = "7"
                if f == "actor":
                    actor = "7"
                if f == "event":
                    event = "7"
                member = i in (MEMBER_PRE, MEMBER_POST)
                edit(i, path=f"/guilds/{guild}/members/{actor}" if member else
                     f"/guilds/{guild}/scheduled-events/{event}")(receipt, records)
            with self.subTest(index=index, field=field):
                self.reject(wrong, "eligibility_request")

    def test_member_path_on_an_event_read_and_the_reverse_refuses(self):
        def cross(receipt, records):
            guild, actor, event = ids(receipt, records)
            edit(MEMBER_PRE, path=f"/guilds/{guild}/scheduled-events/{event}")(receipt, records)
        self.reject(cross, "eligibility_request")
        def reverse(receipt, records):
            guild, actor, event = ids(receipt, records)
            edit(EVENT_PRE, path=f"/guilds/{guild}/members/{actor}")(receipt, records)
        self.reject(reverse, "eligibility_request")

    def test_actor_id_in_an_event_path_refuses(self):
        def swap(receipt, records):
            guild, actor, event = ids(receipt, records)
            edit(EVENT_PRE, path=f"/guilds/{guild}/scheduled-events/{actor}")(receipt, records)
        self.reject(swap, "eligibility_request")

    def test_nonempty_request_payload_hash_refuses_on_every_read(self):
        for index in range(4):
            def stale(receipt, records, i=index):
                set_payload(receipt, records, ordered_ops(receipt)[i], sha(b"{}"))
            with self.subTest(index=index):
                self.reject(stale, "eligibility_request")

    def test_a_plausible_but_wrong_payload_hash_refuses(self):
        def stale(receipt, records):
            set_payload(receipt, records, ordered_ops(receipt)[MEMBER_PRE], "a" * 64)
        self.reject(stale, "eligibility_request")


class Ordering(Eligibility):
    def test_pre_pair_reordered_refuses(self):
        # event pre-read now precedes the member pre-read: event,member,member,event
        self.reject(lambda r, s: move(r, s, EVENT_PRE, "00:07:39"), "eligibility_order")

    def test_post_pair_reordered_refuses(self):
        # member post-read now follows the event post-read: member,event,event,member
        self.reject(lambda r, s: move(r, s, MEMBER_POST, "00:08:21"), "eligibility_order")

    def test_member_pre_read_moved_after_the_insert_refuses_order(self):
        self.reject(lambda r, s: move(r, s, MEMBER_PRE, "00:08:10"), "case_operation_order")

    def test_event_pre_read_after_the_first_insert_refuses(self):
        # The public composition first enforces both pre-reads before both inserts.
        self.reject(lambda r, s: move(r, s, EVENT_PRE, "00:08:10"), "case_operation_order")

    def test_post_reads_cannot_be_moved_before_the_inserts(self):
        # The existing case checker is the lower bound for post-write eligibility.
        for index in (MEMBER_POST, EVENT_POST):
            with self.subTest(index=index):
                self.reject(lambda r, s, i=index: move(r, s, i, "00:07:50"),
                            "case_operation_order")

    def test_read_record_order_does_not_replace_ledger_sequence(self):
        self.accept(lambda r, s: reads(s).reverse())


class Observation(Eligibility):
    def effect_time(self, receipt, index):
        return effects_of(receipt)[ordered_ops(receipt)[index]]["observed_at"]

    def test_received_equal_to_the_attempt_and_the_next_step_is_accepted(self):
        # Baseline sits exactly on every attempt time; equality is allowed (whole seconds).
        receipt, records, now = interaction_fixture()
        for index in range(4):
            self.assertEqual(self.effect_time(receipt, index),
                             read_of(records, ordered_ops(receipt)[index])["received_at"])
        self.assertIsNone(self.check(receipt, records, now))

    def test_received_before_the_attempt_refuses_on_every_read(self):
        from datetime import datetime, timedelta
        for index in range(4):
            def early(receipt, records, i=index):
                stamp = datetime.strptime(self.effect_time(receipt, i), "%Y-%m-%dT%H:%M:%SZ")
                edit(i, received_at=(stamp - timedelta(seconds=1)).strftime("%Y-%m-%dT%H:%M:%SZ"))(
                    receipt, records)
            with self.subTest(index=index):
                self.reject(early, "eligibility_observation")

    def test_received_after_the_next_step_refuses(self):
        # (index, received_at): next step is event pre-read, first insert, event post-read, edit.
        for index, clock in ((MEMBER_PRE, "00:07:41"), (EVENT_PRE, "00:08:01"),
                             (MEMBER_POST, "00:08:21"), (EVENT_POST, "00:08:31")):
            with self.subTest(index=index):
                self.reject(edit(index, received_at=at(clock)), "eligibility_observation")

    def test_received_exactly_at_the_next_step_is_accepted(self):
        self.accept(edit(EVENT_PRE, received_at=at("00:08:00")))
        self.accept(edit(MEMBER_POST, received_at=at("00:08:20")))

    def test_received_at_or_after_the_case_end_refuses(self):
        for index in range(4):
            def late(receipt, records, i=index):
                ended = case_of(receipt, CASE)["ended_at"]
                edit(i, received_at=ended)(receipt, records)
            with self.subTest(index=index):
                self.reject(late, "eligibility_observation")

    def test_received_far_after_the_case_end_refuses(self):
        self.reject(edit(EVENT_POST, received_at=at("23:59:59")), "eligibility_observation")

    def test_wrong_observer_refuses_on_every_read(self):
        for index in range(4):
            with self.subTest(index=index):
                self.reject(edit(index, observer_ref="other-observer"), "eligibility_observation")

    def test_one_wrong_observer_among_correct_ones_refuses(self):
        self.reject(edit(EVENT_POST, observer_ref="other-observer"), "eligibility_observation")

    def test_bad_timestamp_shapes_refuse_at_intake(self):
        for value in ("2026-01-01T00:07:40", "2026-01-01 00:07:40Z", "2026-13-01T00:07:40Z", 5, None):
            with self.subTest(value=value):
                self.refuse_intake(lambda s, v=value: reads(s)[0].update(received_at=v))


class RawBodies(Eligibility):
    def test_malformed_json_refuses_with_the_parser_code(self):
        for text, code in (("{", "json"), ("NaN", "non_json_number"), ("{'a':1}", "json"),
                           ('{"user":{"id":"300"}}x', "json")):
            for index in (MEMBER_PRE, EVENT_POST):
                with self.subTest(text=text, index=index):
                    self.reject(body(index, text), code)

    def test_duplicate_keys_refuse(self):
        for index, text in (
                (MEMBER_PRE, '{"user":{"id":"300"},"user":{"id":"300"}}'),
                (MEMBER_POST, '{"user":{"id":"300","id":"300"}}'),
                (EVENT_PRE, '{"id":"600","id":"600","guild_id":"1","status":2}'),
                (EVENT_POST, '{"id":"600","guild_id":"1","status":2,"status":2}')):
            with self.subTest(index=index):
                self.reject(body(index, text), "duplicate_key")

    def test_non_object_and_scalar_bodies_refuse_before_the_echo_check(self):
        for text in ("[]", "5", '"s"', "true", "null", "[[]]"):
            for index in (MEMBER_PRE, EVENT_PRE):
                with self.subTest(text=text, index=index):
                    self.reject_before_eligibility(body(index, text))

    def test_empty_body_refuses_at_intake(self):
        self.refuse_intake(lambda s: reads(s)[0].update(body_utf8=""))

    def test_extra_profile_fields_on_a_member_refuse_deliberately(self):
        for index in (MEMBER_PRE, MEMBER_POST):
            for value in ({"user": {"id": "300"}, "roles": []},
                          {"user": {"id": "300"}, "nick": None},
                          {"user": {"id": "300", "username": "someone"}},
                          {"user": {"id": "300", "bot": False}},
                          {"user": {"id": "300"}, "extra": 1}):
                with self.subTest(index=index, value=value):
                    self.reject(body(index, value), "object_fields")

    def test_extra_event_fields_refuse_deliberately(self):
        def extended(index, **extra):
            def mutate(receipt, records):
                value = parsed(records, receipt, index)
                value.update(extra)
                body(index, value)(receipt, records)
            return mutate
        for index in (EVENT_PRE, EVENT_POST):
            for extra in ({"name": "x"}, {"description": "x"}, {"creator_id": "1"},
                          {"entity_type": 3}, {"channel_id": None}):
                with self.subTest(index=index, extra=extra):
                    self.reject(extended(index, **extra), "object_fields")

    def test_missing_fields_refuse(self):
        def without(index, *path):
            def mutate(receipt, records):
                value = parsed(records, receipt, index)
                target = value
                for key in path[:-1]:
                    target = target[key]
                del target[path[-1]]
                body(index, value)(receipt, records)
            return mutate
        for index, path in ((MEMBER_PRE, ("user",)), (MEMBER_PRE, ("user", "id")),
                            (EVENT_PRE, ("id",)), (EVENT_PRE, ("guild_id",)),
                            (EVENT_POST, ("status",))):
            with self.subTest(index=index, path=path):
                self.reject(without(index, *path), "object_fields")

    def test_a_member_body_on_an_event_read_and_the_reverse_refuses(self):
        def swap(receipt, records):
            guild, actor, event = ids(receipt, records)
            body(EVENT_PRE, {"user": {"id": actor}})(receipt, records)
        self.reject_before_eligibility(swap)
        def reverse(receipt, records):
            guild, actor, event = ids(receipt, records)
            body(MEMBER_PRE, {"id": event, "guild_id": guild, "status": 2})(receipt, records)
        self.reject_before_eligibility(reverse)

    def test_oversized_body_refuses_at_intake(self):
        for text in ('{"user":{"id":"300"},"pad":"' + "x" * 5000 + '"}', " " * 5000 + "{}"):
            with self.subTest(length=len(text)):
                self.refuse_intake(lambda s, t=text: reads(s)[0].update(body_utf8=t))

    def test_deeply_nested_body_refuses_before_the_echo_check(self):
        self.reject_before_eligibility(body(MEMBER_PRE, "[" * 200 + "]" * 200))
        self.reject_before_eligibility(body(MEMBER_PRE, '{"user":' * 200 + "{}" + "}" * 200))

    def test_non_string_body_types_refuse_at_intake(self):
        for value in (None, 5, ["{}"], {"user": {"id": "300"}}, True):
            with self.subTest(value=repr(value)):
                self.refuse_intake(lambda s, v=value: reads(s)[0].update(body_utf8=v))

    def test_whitespace_around_valid_json_follows_the_raw_bytes(self):
        # Bodies are compared by parsed meaning only; reformatting stays accepted.
        def spaced(receipt, records):
            guild, actor, event = ids(receipt, records)
            body(MEMBER_PRE, ' { "user" : { "id" : "%s" } } ' % actor)(receipt, records)
        self.accept(spaced)


class SchemaClosure(Eligibility):
    def test_read_records_are_closed_at_intake(self):
        self.refuse_intake(lambda s: reads(s)[0].update(extra=1))
        self.refuse_intake(lambda s: reads(s)[0].update(token="x"))
        for field in ("operation_id", "observer_ref", "origin", "http_method", "path",
                      "received_at", "http_status", "body_utf8"):
            with self.subTest(field=field):
                self.refuse_intake(lambda s, f=field: reads(s)[0].pop(f))

    def test_exchange_without_the_eligibility_key_refuses_at_intake(self):
        self.refuse_intake(lambda s: exchange_of(s, CASE).pop("eligibility_reads"))
        self.refuse_intake(lambda s: exchange_of(s, "off-feed-add").pop("eligibility_reads"))

    def test_read_list_must_be_a_list(self):
        for value in (None, {}, "", 0):
            with self.subTest(value=repr(value)):
                self.refuse_intake(lambda s, v=value: exchange_of(s, CASE).update(
                    eligibility_reads=v))

    def test_operation_and_observer_refs_must_be_well_formed(self):
        for field, value in (("operation_id", ""), ("operation_id", "Bad Op"), ("operation_id", 5),
                             ("observer_ref", ""), ("observer_ref", None)):
            with self.subTest(field=field, value=value):
                self.refuse_intake(lambda s, f=field, v=value: reads(s)[0].update({f: v}))


class PlanPin(Eligibility):
    def test_read_edit_without_repinning_the_plan_refuses(self):
        # status 3 is a valid change by itself; only the stale pin may refuse it.
        def valid_but_unpinned(receipt, records):
            guild, actor, event = ids(receipt, records)
            body(EVENT_PRE, {"id": event, "guild_id": guild, "status": 3})(receipt, records)
        self.reject(valid_but_unpinned, "interaction_exchange_content", pin=False)

    def test_the_same_edit_with_a_repinned_plan_is_accepted(self):
        def valid(receipt, records):
            guild, actor, event = ids(receipt, records)
            body(EVENT_PRE, {"id": event, "guild_id": guild, "status": 3})(receipt, records)
        self.accept(valid)

    def test_every_read_field_is_covered_by_the_pin(self):
        for index, field, value in (
                (MEMBER_PRE, "received_at", at("00:07:41")),
                (MEMBER_PRE, "observer_ref", "other-observer"),
                (EVENT_POST, "http_status", 404),
                (EVENT_PRE, "path", "/guilds/7/scheduled-events/7"),
                (MEMBER_POST, "body_utf8", '{"user":{"id":"7"}}')):
            def drift(receipt, records, i=index, f=field, v=value):
                edit(i, **{f: v})(receipt, records)
            with self.subTest(field=field):
                self.reject(drift, "interaction_exchange_content", pin=False)

    def test_reordering_or_dropping_reads_without_repin_refuses_on_the_pin(self):
        self.reject(lambda r, s: reads(s).reverse(), "interaction_exchange_content", pin=False)
        self.reject(lambda r, s: reads(s).pop(), "interaction_exchange_content", pin=False)

    def test_plan_pin_digest_changes_with_a_read_change(self):
        receipt, records, _ = interaction_fixture()
        before = records[PLAN]["data"]["exchanges"]["sha256"]
        edit(EVENT_PRE, http_status=404)(receipt, records)
        repin_interactions(receipt, records)
        self.assertNotEqual(before, records[PLAN]["data"]["exchanges"]["sha256"])


if __name__ == "__main__":
    unittest.main()
