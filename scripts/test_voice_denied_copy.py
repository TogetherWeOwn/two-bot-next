#!/usr/bin/env python3
"""Offline voice-room denied-copy fixtures; stdlib only, no network access."""

import sys
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import voice_denied_copy as denied  # noqa: E402

REPO = Path(__file__).resolve().parent.parent
VOICE_ROOMS = REPO / "crates" / "bot" / "src" / "voice_rooms.rs"
ROOM_HTTP = REPO / "crates" / "discord" / "src" / "voice_rooms.rs"
REPLIES = REPO / "crates" / "core" / "src" / "router" / "replies.rs"

EXPECTED_NAMES = {
    "create_member_denied", "create_bot_denied",
    "create_required_role", "create_command_restricted",
    "join_not_occupant", "join_not_a_room", "join_not_in_room",
    "move_bot_denied", "generic_failure_contrast",
}


class DeniedCopyTests(unittest.TestCase):
    def setUp(self):
        # Any real HTTP is a test failure: the classifier never fetches.
        guard = mock.patch("urllib.request.OpenerDirector.open",
                           side_effect=AssertionError("network access in a unit test"))
        guard.start()
        self.addCleanup(guard.stop)
        self.pack = denied.load_pack()

    def test_pack_shape_is_versioned_with_unique_names(self):
        self.assertEqual(self.pack["version"], 1)
        names = [case["name"] for case in self.pack["cases"]]
        self.assertEqual(len(names), len(set(names)))
        self.assertEqual(set(names), EXPECTED_NAMES)
        for case in self.pack["cases"]:
            self.assertTrue(case["path"] is None or case["path"] in
                            {"create", "join", "move"}, case["name"])
            self.assertTrue(case["expected_outcome"], case["name"])
            self.assertTrue(case["meaning"], case["name"])

    def test_every_fixture_classifies_to_its_expected_outcome(self):
        for name, outcome, expected in denied.classify_pack(self.pack):
            with self.subTest(fixture=name):
                self.assertEqual(outcome, expected)

    def test_denied_cases_never_land_on_generic_failure(self):
        for name, outcome, _ in denied.classify_pack(self.pack):
            with self.subTest(fixture=name):
                if name == "generic_failure_contrast":
                    self.assertEqual(outcome, denied.GENERIC_FAILURE)
                else:
                    self.assertNotEqual(outcome, denied.GENERIC_FAILURE)

    def test_outcomes_are_distinct_per_shape(self):
        seen = {}
        for name, outcome, _ in denied.classify_pack(self.pack):
            key = name if name != "generic_failure_contrast" else "generic_failure"
            self.assertNotIn(outcome, seen,
                             f"{name} collides with {seen.get(outcome)}")
            seen[outcome] = key
        self.assertEqual(len(seen), len(EXPECTED_NAMES))

    def test_exact_replies_reject_prefix_and_suffixed_lookalikes(self):
        # A substring match would let a wrapped or truncated denial pass as
        # the exact copy; the classifier requires byte equality, so partial
        # and padded variants must fall closed to generic_failure.
        for outcome, exact in denied.EXACT_REPLIES.items():
            with self.subTest(outcome=outcome):
                self.assertEqual(
                    denied.classify_observation({"reply": exact}), outcome)
                self.assertEqual(
                    denied.classify_observation({"reply": exact + " "}),
                    denied.GENERIC_FAILURE)
                self.assertEqual(
                    denied.classify_observation({"reply": "Note: " + exact}),
                    denied.GENERIC_FAILURE)
                self.assertEqual(
                    denied.classify_observation({"reply": exact[:-1]}),
                    denied.GENERIC_FAILURE)

    def test_unknown_shapes_fall_closed_to_generic_failure(self):
        self.assertEqual(denied.classify_observation({}), denied.GENERIC_FAILURE)
        self.assertEqual(denied.classify_observation(None), denied.GENERIC_FAILURE)
        self.assertEqual(
            denied.classify_observation({"reply": "Created <#500>"}),
            denied.GENERIC_FAILURE)
        self.assertEqual(
            denied.classify_observation({"reply": "Discord refused the channel create. "
                                                  "Check my permissions and try again."}),
            denied.GENERIC_FAILURE)

    def test_move_line_needs_both_channel_prefix_and_denial(self):
        self.assertEqual(
            denied.classify_observation(
                {"reply": "channel <#500>: " + denied.MOVE_BOT_DENIED_LINE}),
            denied.MOVE_BOT_DENIED)
        # The bare Display without the failure-line channel prefix is not
        # the surfaced shape.
        self.assertEqual(
            denied.classify_observation({"reply": denied.MOVE_BOT_DENIED_LINE}),
            denied.GENERIC_FAILURE)
        self.assertEqual(
            denied.classify_observation({"reply": "channel <#500>: category is full"}),
            denied.GENERIC_FAILURE)

    def test_create_member_denial_matches_the_handler_source(self):
        self.assertIn(denied.CREATE_MEMBER_DENIAL, VOICE_ROOMS.read_text())

    def test_create_bot_denial_matches_the_create_error_source(self):
        self.assertIn(denied.CREATE_BOT_DENIAL, VOICE_ROOMS.read_text())

    def test_role_gate_denials_match_the_access_source(self):
        source = VOICE_ROOMS.read_text()
        self.assertIn(denied.CREATE_REQUIRED_ROLE, source)
        self.assertIn(denied.CREATE_COMMAND_RESTRICTED, source)

    def test_join_denials_match_the_refusal_sources(self):
        source = VOICE_ROOMS.read_text()
        self.assertIn(denied.JOIN_NOT_OCCUPANT, source)
        self.assertIn(denied.JOIN_NOT_A_ROOM, source)
        self.assertIn(denied.JOIN_NOT_IN_ROOM, source)

    def test_move_denial_display_matches_the_http_error_source(self):
        self.assertIn(denied.MOVE_BOT_DENIED_LINE, ROOM_HTTP.read_text())

    def test_generic_prefix_matches_the_reply_contract_source(self):
        self.assertIn("Something went wrong (ref {reference})", REPLIES.read_text())

    def test_pack_carries_no_secret_or_credential(self):
        raw = (REPO / "scripts" / "fixtures" / "voice_denied_copy.json").read_text()
        for token in ("Bot ", "Bearer", "BEGIN ", "DISCORD_TOKEN"):
            self.assertNotIn(token, raw)
        # Only the contrast case may borrow failure copy, and only with the
        # obvious placeholder reference.
        self.assertEqual(raw.count("Something went wrong"), 1)
        self.assertIn("ref 1a2b3c4d", raw)


# A unique marker, readable on purpose so a leak is unmistakable and no
# credential-shaped literal is stored anywhere in this file.
HOSTILE_SENTINEL = "VoiceDeniedNeverEcho4Z"

TRUTHY_NON_STRINGS = (123, -1, 4.5, True, b"reply-bytes", ["not", "a", "string"],
                      {"text": "x"}, ("x",), {"x"})


class HostileObservationTests(unittest.TestCase):
    """Non-string replies never raise and never match a denied arm."""

    def test_non_dict_observations_classify_generic_failure(self):
        for observation in (None, 0, 123, 4.5, True, "reply text", b"reply-bytes",
                            [], ["x"], (), ("x",), {1, 2}):
            with self.subTest(observation=repr(observation)):
                self.assertEqual(denied.classify_observation(observation),
                                 denied.GENERIC_FAILURE)

    def test_non_string_truthy_reply_classifies_generic_failure(self):
        for reply in TRUTHY_NON_STRINGS:
            with self.subTest(reply=repr(reply)):
                self.assertEqual(
                    denied.classify_observation({"reply": reply}),
                    denied.GENERIC_FAILURE)

    def test_reply_containers_holding_denied_surfaces_do_not_match(self):
        # Membership against a container must not trip a denied arm: only a
        # genuine string surface classifies away from generic_failure.
        hostile = ([denied.CREATE_MEMBER_DENIAL], (denied.CREATE_BOT_DENIAL,),
                   {denied.JOIN_NOT_OCCUPANT: True},
                   [denied.MOVE_BOT_DENIED_LINE])
        for reply in hostile:
            with self.subTest(reply=repr(reply)):
                self.assertEqual(denied.classify_observation({"reply": reply}),
                                 denied.GENERIC_FAILURE)

    def test_falsy_non_string_values_still_coerce_to_generic_failure(self):
        for value in (0, 0.0, False, [], {}, (), set(), "", None):
            with self.subTest(value=repr(value)):
                self.assertEqual(
                    denied.classify_observation({"reply": value}),
                    denied.GENERIC_FAILURE)


class HostilePackTests(unittest.TestCase):
    """Malformed packs are fixed refusals that never echo hostile bytes."""

    def assert_fixed_cases_refusal(self, pack, label):
        with self.assertRaises(ValueError, msg=f"no fixed cases refusal in {label}") as caught:
            denied.classify_pack(pack)
        self.assertIn("cases", str(caught.exception),
                      f"pack refusal does not name cases in {label}")
        self.assertNotIn(HOSTILE_SENTINEL, str(caught.exception),
                         f"pack refusal echoes hostile bytes in {label}")

    def assert_fixed_case_refusal(self, pack, label):
        with self.assertRaises(ValueError, msg=f"no fixed case refusal in {label}") as caught:
            denied.classify_pack(pack)
        self.assertIn("case", str(caught.exception).lower(),
                      f"case refusal does not name the case in {label}")
        self.assertNotIn(HOSTILE_SENTINEL, str(caught.exception),
                         f"case refusal echoes hostile bytes in {label}")

    def test_packs_without_cases_are_fixed_refusals(self):
        for pack in ({}, {"version": 1}, {"version": 1, "evil": HOSTILE_SENTINEL}):
            with self.subTest(pack=repr(pack)):
                self.assert_fixed_cases_refusal(pack, repr(pack))

    def test_non_dict_packs_are_fixed_refusals(self):
        for pack in ([], "pack", 123, 4.5, True, b"pack"):
            with self.subTest(pack=repr(pack)):
                self.assert_fixed_cases_refusal(pack, repr(pack))

    def test_non_list_cases_are_fixed_refusals(self):
        for cases in (None, "ab", 123, {"a": 1}, HOSTILE_SENTINEL):
            with self.subTest(cases=repr(cases)):
                self.assert_fixed_cases_refusal({"cases": cases}, repr(cases))

    def test_cases_without_name_are_fixed_refusals(self):
        for case in ({}, {"observation": {"reply": "x"}},
                     {"observation": {}, "evil": HOSTILE_SENTINEL}):
            with self.subTest(case=repr(case)):
                self.assert_fixed_case_refusal({"cases": [case]}, repr(case))

    def test_non_dict_cases_are_fixed_refusals(self):
        for case in (123, "x", None, ["name"], HOSTILE_SENTINEL):
            with self.subTest(case=repr(case)):
                self.assert_fixed_case_refusal({"cases": [case]}, repr(case))

    def test_cases_with_hostile_observations_classify_generic_rows(self):
        pack = {"cases": [
            {"name": "hostile-reply", "observation": {"reply": 123},
             "expected_outcome": "generic_failure"},
            {"name": "hostile-bytes", "observation": {"reply": b"x"},
             "expected_outcome": "generic_failure"},
            {"name": "hostile-container", "observation": {"reply": ["r"]},
             "expected_outcome": "generic_failure"},
        ]}
        self.assertEqual(
            denied.classify_pack(pack),
            [(case["name"], denied.GENERIC_FAILURE, "generic_failure")
             for case in pack["cases"]])

    def test_empty_cases_list_stays_valid(self):
        self.assertEqual(denied.classify_pack({"cases": []}), [])

    def test_none_loads_the_default_pack(self):
        pack = denied.load_pack()
        self.assertEqual(denied.classify_pack(None), denied.classify_pack(pack))
        self.assertEqual(denied.classify_pack(), denied.classify_pack(pack))


if __name__ == "__main__":
    unittest.main()
