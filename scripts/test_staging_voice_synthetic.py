#!/usr/bin/env python3
"""Offline fixtures for scripts/staging_voice_synthetic.py (no network, no token)."""

import json
import os
import socket
import struct
import sys
import threading
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import staging_voice_synthetic as synth  # noqa: E402

ME, CREATOR, ROOM, OTHER, CAT = "900", synth.STAGING_CREATOR_ID, "5001", "5002", "4000"
OWNER_ALLOW = str(synth.OWNER_BITS | (1 << 21))


def obs(**over):
    base = {
        "me": ME,
        "creator": {"id": CREATOR, "parent_id": CAT},
        "room_id": ROOM,
        "room_created": {"id": ROOM, "parent_id": CAT, "name": "🍄 Synthetic’s workshop",
                         "permission_overwrites": [{"id": ME, "type": 1, "allow": OWNER_ALLOW}]},
        "channels": [
            {"id": CREATOR, "parent_id": CAT, "type": 2, "position": 16},
            {"id": ROOM, "parent_id": CAT, "type": 2, "position": 17},
            {"id": OTHER, "parent_id": CAT, "type": 2, "position": 64},
            {"id": "7", "parent_id": CAT, "type": 0, "position": 17},
        ],
        "statuses": ["💬 chatting · 1 person"],
        "deleted": True,
        "delete_seconds": 1.2,
        "grace_seconds": 0,
        "metrics_delta": 1,
    }
    base.update(over)
    return base


def verdicts(observation):
    return {c["check"]: c["verdict"] for c in synth.evaluate(observation)}


class EvaluateTest(unittest.TestCase):
    def test_a_healthy_run_passes_every_check(self):
        self.assertEqual(set(verdicts(obs()).values()), {"PASS"})

    def test_fallback_name_fails_with_either_apostrophe(self):
        for name in ("Synthetic's room", "Synthetic’s room"):
            created = dict(obs()["room_created"], name=name)
            self.assertEqual(verdicts(obs(room_created=created))["name"], "FAIL", name)

    def test_room_after_another_channel_fails_position(self):
        channels = [dict(c) for c in obs()["channels"]]
        channels[1]["position"] = 70  # sorts after OTHER (64)
        self.assertEqual(verdicts(obs(channels=channels))["position"], "FAIL")

    def test_equal_positions_tie_break_by_id(self):
        channels = [dict(c) for c in obs()["channels"]]
        channels[1]["position"] = 16  # ties with the creator; the smaller id sorts first, above it
        self.assertEqual(verdicts(obs(channels=channels))["position"], "FAIL")

    def test_missing_owner_grant_fails(self):
        created = dict(obs()["room_created"], permission_overwrites=[
            {"id": ME, "type": 1, "allow": str(synth.VIEW | synth.CONNECT)}])
        self.assertEqual(verdicts(obs(room_created=created))["owner"], "FAIL")

    def test_role_overwrite_with_the_same_id_is_not_an_owner_grant(self):
        created = dict(obs()["room_created"], permission_overwrites=[
            {"id": ME, "type": 0, "allow": OWNER_ALLOW}])
        self.assertEqual(verdicts(obs(room_created=created))["owner"], "FAIL")

    def test_no_status_line_fails(self):
        self.assertEqual(verdicts(obs(statuses=["", None]))["status"], "FAIL")

    def test_room_left_behind_fails_delete(self):
        self.assertEqual(verdicts(obs(deleted=False))["delete"], "FAIL")

    def test_no_room_fails_create(self):
        result = verdicts(obs(room_id=None, room_created=None))
        self.assertEqual(result["create"], "FAIL")
        self.assertEqual(result["position"], "FAIL")

    def test_metrics_skip_only_when_unavailable(self):
        self.assertEqual(verdicts(obs(metrics_delta=None))["metrics"], "SKIP")
        self.assertEqual(verdicts(obs(metrics_delta=0))["metrics"], "FAIL")


class FenceTest(unittest.TestCase):
    def test_only_the_staging_guild_and_creator_pass(self):
        synth.fence(synth.STAGING_GUILD_ID, synth.STAGING_CREATOR_ID, None)
        synth.fence(synth.STAGING_GUILD_ID, synth.STAGING_CREATOR_ID,
                    "https://two-bot-next-staging.example.workers.dev")
        for guild in (synth.LIVE_GUILD_ID, "1", ""):
            with self.assertRaises(synth.Refusal):
                synth.fence(guild, synth.STAGING_CREATOR_ID, None)
        with self.assertRaises(synth.Refusal):
            synth.fence(synth.STAGING_GUILD_ID, "1546777867978018887", None)
        for url in ("http://two-bot-next-staging.x.workers.dev", "https://two-bot-next-production.x.workers.dev"):
            with self.assertRaises(synth.Refusal):
                synth.fence(synth.STAGING_GUILD_ID, synth.STAGING_CREATOR_ID, url)

    def test_live_guild_refuses_before_any_request(self):
        with mock.patch.object(synth, "Session") as session, mock.patch.object(synth, "rest") as rest, \
                mock.patch.dict(os.environ, {"TWO_VOICE_SYNTHETIC_BOT_TOKEN": "t"}):
            self.assertEqual(synth.main(["--guild", synth.LIVE_GUILD_ID]), 2)
        session.assert_not_called()
        rest.assert_not_called()

    def test_missing_token_refuses(self):
        with mock.patch.dict(os.environ, {"TWO_VOICE_SYNTHETIC_BOT_TOKEN": ""}), \
                mock.patch.object(synth, "Session") as session:
            self.assertEqual(synth.main([]), 2)
        session.assert_not_called()


class CounterTest(unittest.TestCase):
    def test_sums_only_the_requested_outcome(self):
        body = ("# TYPE two_bot_voice_names_total counter\n"
                'two_bot_voice_names_total{outcome="created_with_template"} 3\n'
                'two_bot_voice_names_total{outcome="proposed"} 9\n'
                'two_bot_voice_names_total{outcome="created_with_template",x="y"} 2\n')
        self.assertEqual(synth.counter(body, "created_with_template"), 5)


class WebSocketFrameTest(unittest.TestCase):
    def frame(self, opcode, payload, fin=True):
        head = bytes([(0x80 if fin else 0) | opcode])
        n = len(payload)
        if n < 126:
            head += bytes([n])
        elif n < 65536:
            head += bytes([126]) + struct.pack(">H", n)
        else:
            head += bytes([127]) + struct.pack(">Q", n)
        return head + payload

    def test_fragmented_large_message_and_ping_are_handled(self):
        ours, theirs = socket.socketpair()
        ws = synth.WebSocket.__new__(synth.WebSocket)
        ws.sock, ws.lock = ours, threading.Lock()
        message = json.dumps({"op": 0, "t": "GUILD_CREATE", "d": {"x": "y" * 70000}}).encode()
        theirs.sendall(self.frame(0x9, b"hi") + self.frame(0x1, message[:100], fin=False)
                       + self.frame(0x0, message[100:]))
        got = ws.recv()
        self.assertEqual(got["t"], "GUILD_CREATE")
        pong = theirs.recv(2)
        self.assertEqual(pong[0], 0x8A)  # masked pong answered before the text frame completed
        theirs.sendall(self.frame(0x8, b""))
        self.assertIsNone(ws.recv())
        ours.close()
        theirs.close()

    def test_client_frames_are_masked(self):
        ours, theirs = socket.socketpair()
        ws = synth.WebSocket.__new__(synth.WebSocket)
        ws.sock, ws.lock = ours, threading.Lock()
        ws.send({"op": 1, "d": None})
        head = theirs.recv(2)
        self.assertEqual(head[0], 0x81)
        self.assertTrue(head[1] & 0x80)
        n = head[1] & 0x7F
        mask = theirs.recv(4)
        body = bytes(b ^ mask[i % 4] for i, b in enumerate(theirs.recv(n)))
        self.assertEqual(json.loads(body), {"op": 1, "d": None})
        ours.close()
        theirs.close()


if __name__ == "__main__":
    unittest.main()
