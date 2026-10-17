#!/usr/bin/env python3
"""Offline fixtures for scripts/staging_voice_synthetic.py (no network, no token)."""

import io
import json
import os
import socket
import struct
import sys
import threading
import unittest
import urllib.error
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
        "moved_to": ROOM,
        "rejoin_room_id": "99",
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
        "rename_applied": True,
        "rename_restored": True,
        "limit_initial": 0,
        "limit_probed": 2,
        "limit_set_ok": True,
        "limit_restored": True,
        "privacy_public_baseline": True,
        "privacy_product_bot_manage_roles": True,
        "privacy_no_member_manage_roles": True,
        "privacy_product_bot_role": "Owen QA Test",
        "panel_name": "🍄 Synthetic’s workshop",
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

    def test_existing_creator_rooms_move_the_slot_to_the_end_of_the_block(self):
        # A human's room (owner grant) already follows the creator: the planner
        # groups the new room after it, so creator+1 would be a false failure.
        human = {"id": "5003", "parent_id": CAT, "type": 2, "position": 17,
                 "permission_overwrites": [{"id": "111", "type": 1, "allow": OWNER_ALLOW}]}
        before = [obs()["channels"][0], human, obs()["channels"][2]]
        after = [obs()["channels"][0], human, dict(obs()["channels"][1], position=18), obs()["channels"][2]]
        self.assertEqual(verdicts(obs(channels_before=before, channels=after))["position"], "PASS")
        wrong = [obs()["channels"][0], dict(obs()["channels"][1], position=17), dict(human, position=18),
                 obs()["channels"][2]]
        self.assertEqual(verdicts(obs(channels_before=before, channels=wrong))["position"], "FAIL")

    def test_a_fixed_channel_after_the_creator_is_not_part_of_its_block(self):
        fixed = {"id": "5004", "parent_id": CAT, "type": 2, "position": 20,
                 "permission_overwrites": [{"id": "222", "type": 0, "allow": OWNER_ALLOW}]}
        self.assertEqual(synth.creator_block([obs()["channels"][0], fixed], obs()["creator"]), [])

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

    def test_joiner_not_moved_into_the_room_fails_moved(self):
        self.assertEqual(verdicts(obs(moved_to=None))["moved"], "FAIL")
        self.assertEqual(verdicts(obs(moved_to=OTHER))["moved"], "FAIL")

    def test_hop_back_into_the_creator_without_a_new_room_fails_rejoin(self):
        self.assertEqual(verdicts(obs(rejoin_room_id=None))["rejoin"], "FAIL")
        self.assertEqual(verdicts(obs(rejoin_room_id=ROOM))["rejoin"], "FAIL")
        self.assertEqual(verdicts(obs(rejoin_room_id=CREATOR))["rejoin"], "FAIL")

    def test_no_room_fails_create(self):
        result = verdicts(obs(room_id=None, room_created=None))
        self.assertEqual(result["create"], "FAIL")
        self.assertEqual(result["position"], "FAIL")

    def test_metrics_skip_only_when_unavailable(self):
        self.assertEqual(verdicts(obs(metrics_delta=None))["metrics"], "SKIP")
        self.assertEqual(verdicts(obs(metrics_delta=0))["metrics"], "FAIL")

    def test_rename_passes_when_applied_and_restored(self):
        self.assertEqual(verdicts(obs())["rename"], "PASS")

    def test_rename_skips_when_the_test_bot_may_not_rename(self):
        result = verdicts(obs(rename_applied=False, rename_restored=False, rename_forbidden=True))
        self.assertEqual(result["rename"], "SKIP")

    def test_rename_fails_when_the_original_name_is_not_restored(self):
        result = verdicts(obs(rename_restored=False))
        self.assertEqual(result["rename"], "FAIL")

    def test_rename_fails_when_not_applied(self):
        result = verdicts(obs(rename_applied=False, rename_restored=False))
        self.assertEqual(result["rename"], "FAIL")

    def test_rename_skips_on_the_per_channel_rename_limit(self):
        result = verdicts(obs(rename_applied=False, rename_restored=False,
                              rename_rate_limited=True,
                              rename_note="rename PATCH answered 429 (per-channel rename limit); "
                                          "rename skipped"))
        self.assertEqual(result["rename"], "SKIP")
        detail = next(c["detail"] for c in synth.evaluate(
            obs(rename_applied=False, rename_restored=False, rename_rate_limited=True))
            if c["check"] == "rename")
        self.assertIn("429", detail)

    def test_rename_residue_fails_with_the_note(self):
        result = verdicts(obs(rename_applied=False, rename_restored=False,
                              rename_note="creator already carries synthetic residue"))
        self.assertEqual(result["rename"], "FAIL")
        detail = next(c["detail"] for c in synth.evaluate(
            obs(rename_applied=False, rename_restored=False, rename_note="x"))
            if c["check"] == "rename")
        self.assertIn("x", detail)

    def test_limit_passes_on_verified_set_and_restore(self):
        self.assertEqual(verdicts(obs())["limit"], "PASS")

    def test_limit_fails_when_owner_passes_but_patch_is_forbidden(self):
        result = verdicts(obs(limit_set_ok=False, limit_restored=False, limit_forbidden=True))
        self.assertEqual(result["owner"], "PASS")
        self.assertEqual(result["limit"], "FAIL")
        detail = next(c["detail"] for c in synth.evaluate(
            obs(limit_set_ok=False, limit_restored=False, limit_forbidden=True))
            if c["check"] == "limit")
        self.assertIn("403", detail)
        self.assertIn("Manage Channels", detail)

    def test_limit_skips_on_rate_limit(self):
        result = verdicts(obs(limit_set_ok=False, limit_restored=False,
                              limit_rate_limited=True,
                              limit_note="user_limit PATCH answered 429; limit skipped"))
        self.assertEqual(result["limit"], "SKIP")

    def test_limit_fails_when_not_applied(self):
        result = verdicts(obs(limit_set_ok=False, limit_restored=False))
        self.assertEqual(result["limit"], "FAIL")

    def test_limit_fails_when_set_but_not_restored(self):
        result = verdicts(obs(limit_restored=False))
        self.assertEqual(result["limit"], "FAIL")

    def test_privacy_passes_on_read_only_public_baseline_and_permissions(self):
        self.assertEqual(verdicts(obs())["privacy"], "PASS")

    def test_privacy_fails_when_everyone_connect_is_not_public(self):
        self.assertEqual(verdicts(obs(privacy_public_baseline=False))["privacy"], "FAIL")

    def test_privacy_fails_when_product_bot_role_lacks_manage_roles(self):
        failing = obs(privacy_product_bot_manage_roles=False,
                      privacy_product_bot_role="TWO Staging Product")
        self.assertEqual(verdicts(failing)["privacy"], "FAIL")
        detail = next(c["detail"] for c in synth.evaluate(failing)
                      if c["check"] == "privacy")
        self.assertIn("TWO Staging Product", detail)
        self.assertNotIn("test bot", detail.lower())

    def test_privacy_fails_when_member_overwrite_grants_manage_roles(self):
        self.assertEqual(verdicts(obs(privacy_no_member_manage_roles=False))["privacy"], "FAIL")

    def test_privacy_read_error_fails_and_names_product_role(self):
        result = synth.evaluate(obs(privacy_note=(
            "HTTPError reading room privacy facts; could not verify the product bot's "
            "staging role effective Manage Roles")))
        privacy = next(c for c in result if c["check"] == "privacy")
        self.assertEqual(privacy["verdict"], "FAIL")
        self.assertIn("product bot's staging role", privacy["detail"])
        self.assertNotIn("test bot", privacy["detail"].lower())


    def test_name_panel_passes_on_template_render(self):
        self.assertEqual(verdicts(obs())["name_panel"], "PASS")

    def test_name_panel_fails_on_fallback(self):
        self.assertEqual(verdicts(obs(panel_name="Synthetic's room"))["name_panel"], "FAIL")

    def test_name_panel_fails_when_missing(self):
        self.assertEqual(verdicts(obs(panel_name=None))["name_panel"], "FAIL")

    def test_failed_create_names_the_refusal_reason(self):
        # RED first: a create stall must name its refusal reason and the
        # creator-map state, not just room=None. The next restart-cured stall
        # diagnoses itself from the create line.
        failing = obs(room_id=None, room_created=None, moved_to=None,
                      received_delta=2.0, refused_delta={"unknown_creator": 2.0})
        detail = next(c["detail"] for c in synth.evaluate(failing)
                      if c["check"] == "create")
        self.assertIn("unknown_creator=2", detail)
        self.assertIn("received=2", detail)

    def test_failed_create_without_metrics_carries_no_suffix(self):
        failing = obs(room_id=None, room_created=None, moved_to=None)
        detail = next(c["detail"] for c in synth.evaluate(failing)
                      if c["check"] == "create")
        self.assertNotIn("received=", detail)
        self.assertNotIn("unknown_creator", detail)


class EffectivePermissionsTest(unittest.TestCase):
    GUILD = "700"
    BOT = "800"
    MANAGE_ROLES = 1 << 28
    ADMINISTRATOR = 1 << 3

    def permissions(self, *, everyone=0, roles=None, overwrites=None, owner=None,
                    member_role_ids=None):
        role_rows = [{"id": self.GUILD, "permissions": str(everyone)}]
        role_rows.extend(roles or [])
        if member_role_ids is None:
            member_role_ids = [r["id"] for r in (roles or [])]
        return synth.effective_permissions(
            self.GUILD, self.BOT, member_role_ids, role_rows, overwrites or [],
            guild_owner_id=owner)

    def test_everyone_base_role_contributes_permissions(self):
        result = self.permissions(everyone=self.MANAGE_ROLES)
        self.assertTrue(result & self.MANAGE_ROLES)

    def test_assigned_role_grants_manage_roles(self):
        result = self.permissions(roles=[{"id": "role", "permissions": str(self.MANAGE_ROLES)}])
        self.assertTrue(result & self.MANAGE_ROLES)

    def test_everyone_overwrite_deny_removes_base_permission(self):
        result = self.permissions(
            everyone=self.MANAGE_ROLES,
            overwrites=[{"id": self.GUILD, "type": 0, "allow": "0",
                         "deny": str(self.MANAGE_ROLES)}])
        self.assertFalse(result & self.MANAGE_ROLES)

    def test_everyone_overwrite_allow_grants_permission(self):
        result = self.permissions(
            overwrites=[{"id": self.GUILD, "type": 0, "allow": str(self.MANAGE_ROLES),
                         "deny": "0"}])
        self.assertTrue(result & self.MANAGE_ROLES)

    def test_role_overwrite_allow_grants_permission(self):
        result = self.permissions(
            roles=[{"id": "role", "permissions": "0"}],
            overwrites=[{"id": "role", "type": 0, "allow": str(self.MANAGE_ROLES),
                         "deny": "0"}])
        self.assertTrue(result & self.MANAGE_ROLES)

    def test_unassigned_role_overwrite_does_not_grant_permission(self):
        result = self.permissions(
            roles=[{"id": "other-role", "permissions": "0"}],
            member_role_ids=[],
            overwrites=[{"id": "other-role", "type": 0,
                         "allow": str(self.MANAGE_ROLES), "deny": "0"}])
        self.assertFalse(result & self.MANAGE_ROLES)

    def test_member_overwrite_allow_grants_permission(self):
        result = self.permissions(
            overwrites=[{"id": self.BOT, "type": 1, "allow": str(self.MANAGE_ROLES),
                         "deny": "0"}])
        self.assertTrue(result & self.MANAGE_ROLES)

    def test_role_overwrite_deny_wins_over_role_allow(self):
        result = self.permissions(
            roles=[{"id": "role", "permissions": str(self.MANAGE_ROLES)}],
            overwrites=[{"id": "role", "type": 0, "allow": "0", "deny": str(self.MANAGE_ROLES)}])
        self.assertFalse(result & self.MANAGE_ROLES)

    def test_member_overwrite_deny_wins_over_role_grant(self):
        result = self.permissions(
            roles=[{"id": "role", "permissions": str(self.MANAGE_ROLES)}],
            overwrites=[{"id": self.BOT, "type": 1, "allow": "0", "deny": str(self.MANAGE_ROLES)}])
        self.assertFalse(result & self.MANAGE_ROLES)

    def test_administrator_bypasses_channel_overwrites(self):
        result = self.permissions(
            roles=[{"id": "role", "permissions": str(self.ADMINISTRATOR)}],
            overwrites=[{"id": self.BOT, "type": 1, "allow": "0", "deny": str(self.MANAGE_ROLES)}])
        self.assertTrue(result & self.MANAGE_ROLES)

    def test_guild_owner_bypasses_channel_overwrites(self):
        result = self.permissions(
            overwrites=[{"id": self.BOT, "type": 1, "allow": "0", "deny": str(self.MANAGE_ROLES)}],
            owner=self.BOT)
        self.assertTrue(result & self.MANAGE_ROLES)

    def test_owner_bypass_is_preserved_by_privacy_facts(self):
        roles = [{"id": self.GUILD, "name": "@everyone", "permissions": "0"}]
        channel = {"permission_overwrites": [{
            "id": self.GUILD, "type": 0, "allow": "0",
            "deny": str(self.MANAGE_ROLES)}]}
        member = {"roles": []}
        facts = synth.privacy_facts(
            channel, roles, member, self.GUILD, self.BOT, guild_owner_id=self.BOT)
        self.assertTrue(facts["privacy_product_bot_manage_roles"])
        not_owner = synth.privacy_facts(
            channel, roles, member, self.GUILD, self.BOT, guild_owner_id="another-user")
        self.assertFalse(not_owner["privacy_product_bot_manage_roles"])


class PrivacyFactsTest(unittest.TestCase):
    GUILD = "700"
    PRODUCT_BOT = "800"
    VIEW = 1 << 10
    MANAGE_ROLES = 1 << 28
    CONNECT = 1 << 20

    def facts(self, *, overwrites=None, role_perms=None):
        channel = {"permission_overwrites": overwrites or [
            {"id": self.GUILD, "type": 0, "allow": "0", "deny": "0"}]}
        if role_perms is None:
            role_perms = self.VIEW | self.MANAGE_ROLES
        roles = [
            {"id": self.GUILD, "name": "@everyone", "permissions": "0"},
            {"id": "role", "name": "Staging Product", "permissions": str(role_perms)},
        ]
        member = {"roles": ["role"]}
        return synth.privacy_facts(channel, roles, member, self.GUILD, self.PRODUCT_BOT)

    def test_all_three_privacy_facts_pass_on_public_room(self):
        facts = self.facts()
        self.assertTrue(facts["privacy_public_baseline"])
        self.assertTrue(facts["privacy_product_bot_manage_roles"])
        self.assertTrue(facts["privacy_no_member_manage_roles"])
        self.assertEqual(facts["privacy_product_bot_role"], "Staging Product")

    def test_private_connect_deny_breaks_public_baseline(self):
        facts = self.facts(overwrites=[
            {"id": self.GUILD, "type": 0, "allow": "0", "deny": str(self.CONNECT)}])
        self.assertFalse(facts["privacy_public_baseline"])

    def test_everyone_allow_connect_breaks_public_baseline(self):
        facts = self.facts(overwrites=[
            {"id": self.GUILD, "type": 0, "allow": str(self.CONNECT), "deny": "0"}])
        self.assertFalse(facts["privacy_public_baseline"])

    def test_duplicate_everyone_overwrites_break_public_baseline(self):
        facts = self.facts(overwrites=[
            {"id": self.GUILD, "type": 0, "allow": "0", "deny": "0"},
            {"id": self.GUILD, "type": 0, "allow": "0", "deny": "0"},
        ])
        self.assertFalse(facts["privacy_public_baseline"])

    def test_product_role_without_manage_roles_fails(self):
        facts = self.facts(role_perms=self.VIEW)
        self.assertFalse(facts["privacy_product_bot_manage_roles"])

    def test_product_role_without_view_channel_fails(self):
        # can_edit_overwrites needs VIEW_CHANNEL|MANAGE_ROLES: Manage Roles
        # alone must not read as the /private precondition.
        facts = self.facts(role_perms=self.MANAGE_ROLES)
        self.assertFalse(facts["privacy_product_bot_manage_roles"])

    def test_role_overwrite_allow_grants_the_product_bot_permissions(self):
        facts = self.facts(role_perms=0, overwrites=[
            {"id": self.GUILD, "type": 0, "allow": "0", "deny": "0"},
            {"id": "role", "type": 0,
             "allow": str(self.VIEW | self.MANAGE_ROLES), "deny": "0"},
        ])
        self.assertTrue(facts["privacy_product_bot_manage_roles"])

    def test_role_overwrite_deny_removes_the_product_bot_permission(self):
        facts = self.facts(role_perms=self.VIEW | self.MANAGE_ROLES, overwrites=[
            {"id": self.GUILD, "type": 0, "allow": "0", "deny": "0"},
            {"id": "role", "type": 0, "allow": "0",
             "deny": str(self.MANAGE_ROLES)},
        ])
        self.assertFalse(facts["privacy_product_bot_manage_roles"])

    def test_room_overwrite_deny_removes_the_product_bot_permission(self):
        facts = self.facts(role_perms=self.VIEW | self.MANAGE_ROLES, overwrites=[
            {"id": self.GUILD, "type": 0, "allow": "0",
             "deny": str(self.VIEW | self.MANAGE_ROLES)},
        ])
        self.assertFalse(facts["privacy_product_bot_manage_roles"])

    def test_member_overwrite_allow_grants_the_product_bot_permission(self):
        facts = self.facts(role_perms=0, overwrites=[
            {"id": self.GUILD, "type": 0, "allow": "0", "deny": "0"},
            {"id": self.PRODUCT_BOT, "type": 1,
             "allow": str(self.VIEW | self.MANAGE_ROLES), "deny": "0"},
        ])
        self.assertTrue(facts["privacy_product_bot_manage_roles"])

    def test_member_overwrite_deny_removes_the_product_bot_permission(self):
        facts = self.facts(role_perms=self.VIEW | self.MANAGE_ROLES, overwrites=[
            {"id": self.GUILD, "type": 0, "allow": "0", "deny": "0"},
            {"id": self.PRODUCT_BOT, "type": 1, "allow": "0",
             "deny": str(self.MANAGE_ROLES)},
        ])
        self.assertFalse(facts["privacy_product_bot_manage_roles"])

    def test_other_member_overwrite_does_not_grant_the_product_bot_permission(self):
        facts = self.facts(role_perms=0, overwrites=[
            {"id": self.GUILD, "type": 0, "allow": "0", "deny": "0"},
            {"id": "another-member", "type": 1,
             "allow": str(self.VIEW | self.MANAGE_ROLES), "deny": "0"},
        ])
        self.assertFalse(facts["privacy_product_bot_manage_roles"])
        self.assertFalse(facts["privacy_no_member_manage_roles"])

    def test_any_member_manage_roles_grant_is_reported(self):
        facts = self.facts(overwrites=[
            {"id": self.GUILD, "type": 0, "allow": "0", "deny": "0"},
            {"id": "900", "type": 1, "allow": str(self.MANAGE_ROLES), "deny": "0"},
        ])
        self.assertFalse(facts["privacy_no_member_manage_roles"])


class MetricsSnapshotTest(unittest.TestCase):
    def test_parses_received_and_per_reason_refusals(self):
        body = ("two_bot_voice_joins_received_total 7\n"
                'two_bot_voice_join_refused_total{reason="unknown_creator"} 2\n'
                'two_bot_voice_join_refused_total{reason="halted"} 1\n'
                'two_bot_voice_names_total{outcome="created_with_template"} 3\n')
        seen = []

        def fake_open(req, timeout=None):
            class _Resp:
                def read(self, n):
                    seen.append(n)
                    return body.encode()

                def __enter__(self):
                    return self

                def __exit__(self, *exc):
                    return False
            return _Resp()

        with mock.patch.object(synth.urllib.request, "urlopen", side_effect=fake_open):
            received, refused = synth.metrics_snapshot("https://two-bot-next-staging.x", "tok")
        self.assertEqual(received, 7)
        self.assertEqual(refused, {"unknown_creator": 2.0, "halted": 1.0})
        self.assertEqual(seen, [synth.MAX_BODY_BYTES + 1])

    def test_unavailable_metrics_report_none(self):
        with mock.patch.object(synth.urllib.request, "urlopen",
                               side_effect=OSError("down")):
            self.assertEqual(synth.metrics_snapshot("https://two-bot-next-staging.x", "tok"),
                             (None, None))
        self.assertEqual(synth.metrics_snapshot(None, None), (None, None))


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


class BodyCapTest(unittest.TestCase):
    EXPECTED_UA = "two-bot-voice-synthetic (staging)"

    def padded(self, value, size):
        raw = json.dumps(value).encode()
        self.assertLessEqual(len(raw), size, "fixture must fit the target size")
        return raw + b" " * (size - len(raw))

    def response(self, payload, seen, status=200):
        class _Resp:
            def read(self, n):
                seen.append(n)
                return payload

            def __enter__(self):
                return self

            def __exit__(self, *exc):
                return False

        resp = _Resp()
        resp.status = status
        return resp

    def http_error(self, payload, seen, code=500):
        err = urllib.error.HTTPError("https://discord.com/api/v10/x", code,
                                     "Server Error", {}, io.BytesIO(b""))
        def _read(n, _payload=payload):
            seen.append(n)
            return _payload
        err.read = _read
        return err

    def test_cap_matches_the_reference_probes(self):
        self.assertEqual(synth.MAX_BODY_BYTES, 64 * 1024)
        self.assertEqual(synth.BODY_CAP, synth.MAX_BODY_BYTES)
        import staging_probe
        self.assertEqual(synth.MAX_BODY_BYTES, staging_probe.MAX_BODY_BYTES)

    def test_rest_exact_cap_valid_input_passes(self):
        seen = []
        body = self.padded({"id": ROOM}, synth.MAX_BODY_BYTES)
        self.assertEqual(len(body), synth.MAX_BODY_BYTES)
        with mock.patch.object(synth.urllib.request, "urlopen",
                               return_value=self.response(body, seen)):
            self.assertEqual(synth.rest("t", f"/channels/{ROOM}")["id"], ROOM)
        self.assertEqual(seen, [synth.MAX_BODY_BYTES + 1])

    def test_rest_over_cap_refuses_before_parsing(self):
        sentinel = "SENTINEL_BODY_voice_synthetic_padded"
        base = json.dumps({"id": ROOM, "note": sentinel}).encode()
        body = base + b" " * (synth.MAX_BODY_BYTES + 1 - len(base))
        self.assertEqual(len(body), synth.MAX_BODY_BYTES + 1)
        # Still valid JSON (trailing whitespace): the old parser would accept.
        self.assertEqual(json.loads(body)["note"], sentinel)
        seen = []
        with mock.patch.object(synth.urllib.request, "urlopen",
                               return_value=self.response(body, seen)):
            with self.assertRaises(synth.BodyTooLarge) as ctx:
                synth.rest("t", f"/channels/{ROOM}")
        self.assertEqual(str(ctx.exception), "Discord REST read answered over the body cap")
        self.assertNotIn(sentinel, str(ctx.exception))
        self.assertEqual(seen, [synth.MAX_BODY_BYTES + 1])

    def test_rest_hidden_tail_past_a_valid_prefix_refuses(self):
        prefix = self.padded({"id": ROOM}, synth.MAX_BODY_BYTES)
        self.assertEqual(json.loads(prefix)["id"], ROOM)
        seen = []
        with mock.patch.object(synth.urllib.request, "urlopen",
                               return_value=self.response(prefix + b"X", seen)):
            with self.assertRaises(synth.BodyTooLarge) as ctx:
                synth.rest("t", f"/channels/{ROOM}")
        self.assertEqual(str(ctx.exception), "Discord REST read answered over the body cap")
        self.assertEqual(seen, [synth.MAX_BODY_BYTES + 1])

    def test_rest_http_error_path_is_bounded(self):
        seen = []
        big = b"y" * (synth.MAX_BODY_BYTES + 1)
        with mock.patch.object(synth.urllib.request, "urlopen",
                               side_effect=self.http_error(big, seen)):
            with self.assertRaises(synth.BodyTooLarge) as ctx:
                synth.rest("t", f"/channels/{ROOM}")
        self.assertEqual(str(ctx.exception), "Discord REST read answered over the body cap")
        self.assertEqual(seen, [synth.MAX_BODY_BYTES + 1])
        seen.clear()
        with mock.patch.object(synth.urllib.request, "urlopen",
                               side_effect=self.http_error(b"{}", seen)):
            with self.assertRaises(urllib.error.HTTPError):
                synth.rest("t", f"/channels/{ROOM}")
        self.assertEqual(seen, [synth.MAX_BODY_BYTES + 1])

    def test_metrics_exact_cap_passes(self):
        seen = []
        line = 'two_bot_voice_names_total{outcome="created_with_template"} 4\n'
        body = line.encode() + b"# pad\n" * 10
        body = body + b" " * (synth.MAX_BODY_BYTES - len(body))
        self.assertEqual(len(body), synth.MAX_BODY_BYTES)
        with mock.patch.object(synth.urllib.request, "urlopen",
                               return_value=self.response(body, seen)):
            self.assertEqual(synth.metrics_value("https://two-bot-next-staging.x", "tok"), 4)
        self.assertEqual(seen, [synth.MAX_BODY_BYTES + 1])

    def test_metrics_over_cap_refuses_instead_of_silent_skip(self):
        sentinel = "SENTINEL_BODY_voice_metrics_padded"
        base = (f'# {sentinel}\ntwo_bot_voice_names_total{{outcome="created_with_template"}} 9\n').encode()
        body = base + b" " * (synth.MAX_BODY_BYTES + 1 - len(base))
        self.assertEqual(len(body), synth.MAX_BODY_BYTES + 1)
        seen = []
        with mock.patch.object(synth.urllib.request, "urlopen",
                               return_value=self.response(body, seen)):
            with self.assertRaises(synth.BodyTooLarge) as ctx:
                synth.metrics_value("https://two-bot-next-staging.x", "tok")
        self.assertEqual(str(ctx.exception), "metrics read answered over the body cap")
        self.assertNotIn(sentinel, str(ctx.exception))
        self.assertEqual(seen, [synth.MAX_BODY_BYTES + 1])

    def test_metrics_http_error_path_is_bounded(self):
        seen = []
        big = b"z" * (synth.MAX_BODY_BYTES + 1)
        with mock.patch.object(synth.urllib.request, "urlopen",
                               side_effect=self.http_error(big, seen)):
            with self.assertRaises(synth.BodyTooLarge):
                synth.metrics_value("https://two-bot-next-staging.x", "tok")
        self.assertEqual(seen, [synth.MAX_BODY_BYTES + 1])
        seen.clear()
        with mock.patch.object(synth.urllib.request, "urlopen",
                               side_effect=self.http_error(b"oops", seen)):
            self.assertIsNone(synth.metrics_value("https://two-bot-next-staging.x", "tok"))
        self.assertEqual(seen, [synth.MAX_BODY_BYTES + 1])

    def test_patch_reports_status_without_raising(self):
        seen = []
        body = json.dumps({"id": CREATOR, "name": "Create a Lobby"}).encode()
        with mock.patch.object(synth.urllib.request, "urlopen",
                               return_value=self.response(body, seen)):
            self.assertEqual(synth.rest_patch("t", f"/channels/{CREATOR}", {"name": "x"}),
                             (200, {"id": CREATOR, "name": "Create a Lobby"}))
        self.assertEqual(seen, [synth.MAX_BODY_BYTES + 1])

    def test_patch_forbidden_reports_403(self):
        seen = []
        with mock.patch.object(synth.urllib.request, "urlopen",
                               side_effect=self.http_error(b"{}", seen, code=403)):
            self.assertEqual(synth.rest_patch("t", f"/channels/{CREATOR}", {"name": "x"}),
                             (403, None))
        self.assertEqual(seen, [synth.MAX_BODY_BYTES + 1])

    def test_patch_over_cap_refuses(self):
        big = b"y" * (synth.MAX_BODY_BYTES + 1)
        with mock.patch.object(synth.urllib.request, "urlopen",
                               return_value=self.response(big, [])):
            with self.assertRaises(synth.BodyTooLarge) as ctx:
                synth.rest_patch("t", f"/channels/{CREATOR}", {"name": "x"})
        self.assertEqual(str(ctx.exception), "Discord REST read answered over the body cap")

    def test_user_agent_pins_are_unchanged(self):
        requests = []

        def fake_open(req, timeout=None):
            requests.append(req)
            return self.response(b"{}", [])

        with mock.patch.object(synth.urllib.request, "urlopen", side_effect=fake_open):
            synth.rest("t", f"/channels/{ROOM}")
            synth.rest_patch("t", f"/channels/{ROOM}", {"name": "x"})
            synth.metrics_value("https://two-bot-next-staging.x", "tok")
        self.assertEqual(len(requests), 3)
        for req in requests:
            self.assertEqual(req.get_header("User-agent"), self.EXPECTED_UA)


if __name__ == "__main__":
    unittest.main()
