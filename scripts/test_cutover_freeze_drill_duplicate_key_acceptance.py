"""Protected acceptance: freeze drill refuses duplicate JSON keys.

Offline only (loopback doubles, no Discord, no network beyond 127.0.0.1).
No implementation source or diff is used to derive expectations. The
combined fix must retain this file byte-identically; the builder must make
these tests pass without editing it.

Contract (baseline 8978f1249): live Discord replies are decoded with bare
json.loads, so a repeated key last-wins before absence proofs, identity
checks and verdict grading. Fixed code uses
object_pairs_hook=_reject_duplicate_keys (precedent:
rollback_readiness_probe, qa_cutover_probes) and refuses with fixed
vocabulary containing "duplicate", echoing no raw key, value, body or
header. Refused replies grade nothing; valid replies keep byte-identical
verdicts and the drill still restores.
"""

import copy
import json
import os
import sys
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import test_cutover_freeze_drill as fixtures

drill = fixtures.drill
STAGING_GUILD = fixtures.STAGING_GUILD
CHANNEL = fixtures.CHANNEL
CHANNEL_PATH = fixtures.CHANNEL_PATH
MESSAGES_PATH = fixtures.MESSAGES_PATH
NOTICE_PATH = fixtures.NOTICE_PATH
COMMANDS_PATH = fixtures.COMMANDS_PATH
NOTICE_ID = fixtures.NOTICE_ID
TOKEN = fixtures.TOKEN
REASON = fixtures.REASON
CHANNEL_JSON = fixtures.CHANNEL_JSON

SENTINEL = "SENTINEL_DUP_FREEZE_2b8e5f4a"
HEADER_SENTINEL = "SENTINEL_DUP_HEADER_9c4a1e7d"


def channel_bytes(**overrides):
    base = dict(CHANNEL_JSON)
    base.update(overrides)
    return json.dumps(base).encode()


def channel_dup_bytes(first_name: str, second_name: str) -> bytes:
    base = dict(CHANNEL_JSON)
    raw = (b'{"id": "%s", "guild_id": "%s", "name": "%s", "name": "%s", '
           b'"rate_limit_per_user": %d, "permission_overwrites": %s}') % (
               base["id"].encode(), base["guild_id"].encode(),
               first_name.encode(), second_name.encode(),
               base["rate_limit_per_user"],
               json.dumps(base["permission_overwrites"]).encode())
    return raw


def channel_nested_dup_bytes() -> bytes:
    # Duplicate "allow" inside a foreign overwrite; the survivor is valid
    # and foreign rows are skipped, so bare loads still passes today.
    overwrites = ('[{"id": "5555555555555555555", "type": 0, '
                  '"allow": "%s", "allow": "999", "deny": "888"}]' % SENTINEL).encode()
    base = dict(CHANNEL_JSON)
    raw = (b'{"id": "%s", "guild_id": "%s", "name": "cutover-drill", '
           b'"rate_limit_per_user": 0, "permission_overwrites": %s}') % (
               base["id"].encode(), base["guild_id"].encode(), overwrites)
    return raw


def notice_dup_bytes(first_id: str, second_id: str) -> bytes:
    return ('{"id": "%s", "id": "%s"}' % (first_id, second_id)).encode()


def notice_valid_dup_extra(first_extra: str, second_extra: str) -> bytes:
    return ('{"id": "%s", "channel_id": "%s", "extra": "%s", "extra": "%s"}'
            % (NOTICE_ID, CHANNEL, first_extra, second_extra)).encode()


def commands_dup_bytes(first_extra: str, second_extra: str) -> bytes:
    return ('[{"id": "1", "name": "ping", "extra": "%s", "extra": "%s"}]'
            % (first_extra, second_extra)).encode()


def commands_nested_dup_bytes() -> bytes:
    return ('[{"id": "1", "id": "1", "name": "ping", "note": "%s"}]'
            % SENTINEL).encode()


class FreezeDrillDuplicateKeyAcceptanceTests(unittest.TestCase):
    def setUp(self):
        self.origin = fixtures.LoopbackDiscord()
        self.addCleanup(self.origin.close)
        self.origin.routes = fixtures.discord_routes({})
        for patcher in (mock.patch.dict(os.environ, {"no_proxy": "127.0.0.1"}),
                        mock.patch.object(drill, "API", self.origin.base + "/api/v10")):
            patcher.start()
            self.addCleanup(patcher.stop)

    def call(self):
        return drill.live_transport(TOKEN)

    def run_drill(self):
        result = drill.run_drill(STAGING_GUILD, CHANNEL, REASON, self.call())
        blob = json.dumps(result)
        self.assertNotIn(TOKEN, blob)
        self.assertNotIn(SENTINEL, blob)
        self.assertNotIn(HEADER_SENTINEL, blob)
        self.assertNotIn("SENTINEL_DUP", blob)
        return result

    def assert_duplicate_error(self, ctx):
        self.assertIn("duplicate", str(ctx.exception).lower())
        self.assertNotIn(SENTINEL, str(ctx.exception))
        self.assertNotIn(HEADER_SENTINEL, str(ctx.exception))
        self.assertNotIn("SENTINEL_DUP", str(ctx.exception))

    # -- controls ------------------------------------------------------------

    def test_valid_mock_drill_passes(self):
        outcome = drill.run_drill(STAGING_GUILD, "mock-drill-channel", REASON,
                                  drill.mock_transport())
        self.assertEqual(outcome["result"], "pass")

    def test_valid_live_drill_passes(self):
        result = self.run_drill()
        self.assertEqual(result["result"], "pass")
        self.assertTrue(result["restore"]["restored"])

    # -- direct: channel reads refuse ----------------------------------------

    def test_direct_channel_conflicting_both_orders_refused(self):
        for first, second in (("cutover-drill", "other-valid-name"),
                              ("other-valid-name", "cutover-drill")):
            with self.subTest(first=first, second=second):
                self.origin.routes = fixtures.discord_routes({
                    ("GET", CHANNEL_PATH): (200, {"X-Dup": HEADER_SENTINEL},
                                            channel_dup_bytes(first, second))})
                with self.assertRaises(drill.DrillError) as ctx:
                    self.call()("get_channel", channel_id=CHANNEL)
                self.assert_duplicate_error(ctx)

    def test_direct_channel_overwritten_sentinel_refused_without_echo(self):
        self.origin.routes = fixtures.discord_routes({
            ("GET", CHANNEL_PATH): (200, {"X-Dup": HEADER_SENTINEL},
                                    channel_dup_bytes(SENTINEL, "cutover-drill"))})
        with self.assertRaises(drill.DrillError) as ctx:
            self.call()("get_channel", channel_id=CHANNEL)
        self.assert_duplicate_error(ctx)

    def test_direct_channel_identical_values_refused(self):
        self.origin.routes = fixtures.discord_routes({
            ("GET", CHANNEL_PATH): (200, {}, channel_dup_bytes("cutover-drill",
                                                               "cutover-drill"))})
        with self.assertRaises(drill.DrillError) as ctx:
            self.call()("get_channel", channel_id=CHANNEL)
        self.assertIn("duplicate", str(ctx.exception).lower())

    def test_direct_channel_nested_duplicate_refused(self):
        raw = channel_nested_dup_bytes()
        # Bare loads still skips the foreign row today.
        self.assertEqual(json.loads(raw)["permission_overwrites"][0]["allow"], "999")
        self.origin.routes = fixtures.discord_routes({
            ("GET", CHANNEL_PATH): (200, {}, raw)})
        with self.assertRaises(drill.DrillError) as ctx:
            self.call()("get_channel", channel_id=CHANNEL)
        self.assert_duplicate_error(ctx)

    # -- direct: notice replies refuse ---------------------------------------

    def test_direct_post_notice_conflicting_both_orders_refused(self):
        for first, second in ((SENTINEL, NOTICE_ID), (NOTICE_ID, SENTINEL)):
            with self.subTest(first=first[:12], second=second[:12]):
                self.origin.routes = fixtures.discord_routes({
                    ("POST", MESSAGES_PATH): (200, {"X-Dup": HEADER_SENTINEL},
                                              notice_dup_bytes(first, second))})
                with self.assertRaises(drill.DrillError) as ctx:
                    self.call()("post_notice", channel_id=CHANNEL, content="x")
                self.assert_duplicate_error(ctx)

    def test_direct_post_notice_identical_refused(self):
        self.origin.routes = fixtures.discord_routes({
            ("POST", MESSAGES_PATH): (200, {}, notice_dup_bytes(NOTICE_ID, NOTICE_ID))})
        with self.assertRaises(drill.DrillError) as ctx:
            self.call()("post_notice", channel_id=CHANNEL, content="x")
        self.assertIn("duplicate", str(ctx.exception).lower())

    def test_direct_get_notice_duplicate_refused(self):
        self.origin.routes = fixtures.discord_routes({
            ("GET", NOTICE_PATH): (200, {},
                                   notice_valid_dup_extra(SENTINEL, "valid"))})
        with self.assertRaises(drill.DrillError) as ctx:
            self.call()("get_notice", channel_id=CHANNEL, notice_id=NOTICE_ID)
        self.assert_duplicate_error(ctx)

    # -- end-to-end: refused reads fail closed and still restore -------------

    def assert_failed_closed(self, result, failed_step: str):
        self.assertEqual(result["result"], "fail")
        failed = [s["name"] for s in result["steps"] if s["result"] == "fail"]
        self.assertIn(failed_step, failed)
        detail = next(s["detail"] for s in result["steps"]
                      if s["name"] == failed_step and s["result"] == "fail")
        self.assertIn("duplicate", detail.lower())
        self.assertNotIn(SENTINEL, json.dumps(result))

    def test_end_to_end_channel_duplicate_fails_baseline(self):
        self.origin.routes = fixtures.discord_routes({
            ("GET", CHANNEL_PATH): (200, {},
                                    channel_dup_bytes(SENTINEL, "cutover-drill"))})
        result = self.run_drill()
        self.assert_failed_closed(result, "baseline-read")

    def test_end_to_end_channel_nested_duplicate_fails_baseline(self):
        self.origin.routes = fixtures.discord_routes({
            ("GET", CHANNEL_PATH): (200, {}, channel_nested_dup_bytes())})
        result = self.run_drill()
        self.assert_failed_closed(result, "baseline-read")

    def test_end_to_end_notice_read_duplicate_fails_post(self):
        # POST still creates via stateful; the readback carries the duplicate.
        channel = copy.deepcopy(CHANNEL_JSON)
        channel["id"] = CHANNEL
        routes = fixtures.discord_routes({
            ("GET", CHANNEL_PATH): (200, {}, json.dumps(channel).encode()),
            ("GET", NOTICE_PATH): (200, {},
                                   notice_valid_dup_extra("a", SENTINEL)),
        })
        self.origin.routes = routes
        result = self.run_drill()
        # The notice identity can never be proven from refused bytes.
        self.assert_failed_closed(result, "freeze-notice-post")

    def test_end_to_end_commands_duplicate_fails_surface_verify(self):
        self.origin.routes = fixtures.discord_routes({
            ("GET", COMMANDS_PATH): (200, {},
                                     commands_dup_bytes("a", SENTINEL))})
        result = self.run_drill()
        self.assert_failed_closed(result, "command-surface-verify")

    def test_end_to_end_commands_nested_duplicate_fails_surface_verify(self):
        self.origin.routes = fixtures.discord_routes({
            ("GET", COMMANDS_PATH): (200, {}, commands_nested_dup_bytes())})
        result = self.run_drill()
        self.assert_failed_closed(result, "command-surface-verify")


if __name__ == "__main__":
    unittest.main()
