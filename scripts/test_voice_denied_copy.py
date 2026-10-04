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


if __name__ == "__main__":
    unittest.main()
