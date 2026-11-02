#!/usr/bin/env python3
"""Offline fixtures for scripts/staging_voice_synthetic.py (no network, no token)."""

import contextlib
import io
import json
import math
import os
import socket
import struct
import sys
import tempfile
import threading
import time
import unittest
import urllib.error
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import staging_voice_synthetic as synth  # noqa: E402
import test_staging_voice_synthetic_owned_room_acceptance as owned

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

    def test_a_predecessor_swept_mid_run_does_not_fail_position(self):
        # The empty-room sweep deleted the snapshotted predecessor between the
        # before/after channel reads: the room directly below the creator is
        # correctly placed, and the detail names the swept predecessor.
        human = {"id": "5003", "parent_id": CAT, "type": 2, "position": 17,
                 "permission_overwrites": [{"id": "111", "type": 1, "allow": OWNER_ALLOW}]}
        before = [obs()["channels"][0], human, obs()["channels"][2]]
        after = [obs()["channels"][0], obs()["channels"][1], obs()["channels"][2]]
        result = synth.evaluate(obs(channels_before=before, channels=after))
        position = next(c for c in result if c["check"] == "position")
        self.assertEqual(position["verdict"], "PASS")
        self.assertIn("swept since the snapshot: ['5003']", position["detail"])

    def test_only_surviving_predecessors_anchor_the_slot(self):
        # Of two snapshotted predecessors one is swept mid-run: the room must
        # follow the survivor, not the ghost and not the creator.
        swept = {"id": "5003", "parent_id": CAT, "type": 2, "position": 17,
                 "permission_overwrites": [{"id": "111", "type": 1, "allow": OWNER_ALLOW}]}
        stayed = {"id": "5005", "parent_id": CAT, "type": 2, "position": 18,
                  "permission_overwrites": [{"id": "113", "type": 1, "allow": OWNER_ALLOW}]}
        before = [obs()["channels"][0], swept, stayed, obs()["channels"][2]]
        good = [obs()["channels"][0], dict(stayed, position=17),
                dict(obs()["channels"][1], position=18), obs()["channels"][2]]
        self.assertEqual(verdicts(obs(channels_before=before, channels=good))["position"], "PASS")
        bad = [obs()["channels"][0], dict(obs()["channels"][1], position=17),
               dict(stayed, position=18), obs()["channels"][2]]
        self.assertEqual(verdicts(obs(channels_before=before, channels=bad))["position"], "FAIL")

    def test_a_swept_predecessor_does_not_excuse_a_misplaced_room(self):
        # The predecessor is gone but the room landed after an unrelated
        # channel instead of below the creator: still a misplacement.
        human = {"id": "5003", "parent_id": CAT, "type": 2, "position": 17,
                 "permission_overwrites": [{"id": "111", "type": 1, "allow": OWNER_ALLOW}]}
        before = [obs()["channels"][0], human, obs()["channels"][2]]
        after = [obs()["channels"][0], obs()["channels"][2], dict(obs()["channels"][1], position=70)]
        self.assertEqual(verdicts(obs(channels_before=before, channels=after))["position"], "FAIL")

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

    def test_everyone_allow_connect_keeps_public_baseline(self):
        # Private is an @everyone Connect *deny* (everyone_connect_overwrite
        # in private_runtime.rs); a public room inherits its source's
        # overwrites, which commonly allow Connect explicitly. An allow
        # alone must not read as private.
        facts = self.facts(overwrites=[
            {"id": self.GUILD, "type": 0, "allow": str(self.CONNECT), "deny": "0"}])
        self.assertTrue(facts["privacy_public_baseline"])

    def test_privacy_facts_report_everyone_allow_deny_and_count(self):
        facts = self.facts(overwrites=[
            {"id": self.GUILD, "type": 0,
             "allow": str(self.VIEW | self.CONNECT), "deny": str(self.CONNECT)}])
        self.assertEqual(facts["privacy_everyone_allow"], self.VIEW | self.CONNECT)
        self.assertEqual(facts["privacy_everyone_deny"], self.CONNECT)
        self.assertEqual(facts["privacy_everyone_count"], 1)
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

        with mock.patch.object(synth, "_open_no_redirect", side_effect=fake_open):
            received, refused = synth.metrics_snapshot("https://two-bot-next-staging.x", "tok")
        self.assertEqual(received, 7)
        self.assertEqual(refused, {"unknown_creator": 2.0, "halted": 1.0})
        self.assertEqual(seen, [synth.MAX_BODY_BYTES + 1])

    def test_unavailable_metrics_report_none(self):
        with mock.patch.object(synth, "_open_no_redirect",
                               side_effect=OSError("down")):
            self.assertEqual(synth.metrics_snapshot("https://two-bot-next-staging.x", "tok"),
                             (None, None))
        self.assertEqual(synth.metrics_snapshot(None, None), (None, None))


class TimingInputTest(unittest.TestCase):
    BAD = ("nan", "NaN", "inf", "-inf", "0", "-1", "0.0", "601", "1e999", "abc", "")

    def parse_times(self, argv, grace_env=None):
        """Parse the four timing flags with the env-sourced grace default."""
        env = {} if grace_env is None else {
            "TWO_STAGING_EMPTY_GRACE_SECONDS": grace_env}
        with mock.patch.dict(os.environ, env, clear=False):
            if grace_env is None:
                os.environ.pop("TWO_STAGING_EMPTY_GRACE_SECONDS", None)
            stderr = io.StringIO()
            try:
                with contextlib.redirect_stderr(stderr):
                    args = synth.parse_args(argv)
            except SystemExit as exc:
                return exc.code, stderr.getvalue(), None
        return 0, stderr.getvalue(), args

    def refused(self, argv, grace_env=None):
        """A bad timing value refuses with exit 2 and the fixed message."""
        code, stderr, _ = self.parse_times(argv, grace_env)
        self.assertEqual(code, 2)
        self.assertIn(synth.TIMING_REFUSAL, stderr)

    def test_all_four_flags_share_one_bounded_type(self):
        for flag in ("--create-within", "--settle", "--grace", "--delete-slack"):
            for value in self.BAD:
                with self.subTest(flag=flag, value=value):
                    # A bare -inf reads as an option flag to argparse (its
                    # negative-number matcher only allows numerics), so the
                    # same value goes through the --flag=value form.
                    argv = [f"{flag}={value}"] if value == "-inf" else [flag, value]
                    self.refused(argv)

    def test_refusal_names_no_input_value(self):
        # Fixed vocabulary only: for one flag the echo must not vary with
        # the rejected value (argparse prefixes the flag name by design).
        _, first, _ = self.parse_times(["--settle", "nan"])
        _, second, _ = self.parse_times(["--settle", "-3"])
        self.assertEqual(first, second)
        self.assertNotIn("nan", first)
        self.assertNotIn("-3", second)

    def test_refusal_happens_before_any_live_call(self):
        with mock.patch.object(synth, "Session") as session, \
                mock.patch.object(synth, "rest") as rest, \
                mock.patch.dict(os.environ, {
                    "TWO_VOICE_SYNTHETIC_BOT_TOKEN": "t",
                    "TWO_STAGING_EMPTY_GRACE_SECONDS": "nan"}):
            stderr = io.StringIO()
            with contextlib.redirect_stderr(stderr):
                with self.assertRaises(SystemExit) as ctx:
                    synth.main([])
        self.assertEqual(ctx.exception.code, 2)
        self.assertIn(synth.TIMING_REFUSAL, stderr.getvalue())
        session.assert_not_called()
        rest.assert_not_called()

    def test_env_sourced_grace_uses_the_same_type(self):
        for value in self.BAD:
            with self.subTest(value=value):
                self.refused([], value)
        code, _, args = self.parse_times([], "45")
        self.assertEqual(code, 0)
        self.assertEqual(args.grace, 45.0)

    def test_defaults_and_cap_boundary_pass(self):
        code, _, args = self.parse_times([])
        self.assertEqual(code, 0)
        self.assertEqual(
            (args.create_within, args.settle, args.grace, args.delete_slack),
            (20.0, 8.0, 60.0, 20.0))
        code, _, args = self.parse_times(
            ["--create-within", "600", "--settle", "600",
             "--grace", "600", "--delete-slack", "600"])
        self.assertEqual(code, 0)
        self.assertTrue(all(v == 600.0 for v in
                             (args.create_within, args.settle, args.grace,
                              args.delete_slack)))

    def test_bad_flag_refuses_in_main_before_any_live_call(self):
        with mock.patch.dict(os.environ, {"TWO_VOICE_SYNTHETIC_BOT_TOKEN": "t"}), \
                mock.patch.object(synth, "run_live") as live:
            stderr = io.StringIO()
            with contextlib.redirect_stderr(stderr):
                with self.assertRaises(SystemExit) as ctx:
                    synth.main(["--settle", "nan"])
        self.assertEqual(ctx.exception.code, 2)
        self.assertIn(synth.TIMING_REFUSAL, stderr.getvalue())
        live.assert_not_called()

    def test_evidence_from_a_live_run_carries_only_finite_values(self):
        # json.dump would serialize NaN/inf as bare literals, so the evidence
        # file is parsed strictly: any non-finite constant refuses, which the
        # bounded timing type plus this parse make unreachable.
        def strict(raw):
            return json.loads(raw, parse_constant=lambda c: (_ for _ in ()).throw(
                ValueError(f"non-finite evidence value: {c}")))
        with tempfile.TemporaryDirectory() as tmp:
            out = str(Path(tmp) / "out.json")
            with mock.patch.dict(os.environ, {"TWO_VOICE_SYNTHETIC_BOT_TOKEN": "t"}), \
                    mock.patch.object(synth, "run_live", return_value=obs()) as live, \
                    contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(synth.main(["--evidence", out]), 0)
            live.assert_called_once()
            parsed = strict(Path(out).read_text())
        self.assertEqual(parsed["guild"], synth.STAGING_GUILD_ID)
        floats = [value for check in parsed["checks"] for value in check.values()
                  if isinstance(value, float)]
        self.assertTrue(floats or True)  # shape guard: no crash when none
        self.assertTrue(all(math.isfinite(value) for value in floats))


class FenceTest(unittest.TestCase):
    def test_only_the_staging_guild_and_creator_pass(self):
        synth.fence(synth.STAGING_GUILD_ID, synth.STAGING_CREATOR_ID, None)
        synth.fence(synth.STAGING_GUILD_ID, synth.STAGING_CREATOR_ID,
                    "https://two-bot-next-staging.example.workers.dev")
        synth.fence(synth.STAGING_GUILD_ID, synth.STAGING_CREATOR_ID,
                    "https://two-bot-next-staging.example.workers.dev/")
        for guild in (synth.LIVE_GUILD_ID, "1", ""):
            with self.assertRaises(synth.Refusal):
                synth.fence(guild, synth.STAGING_CREATOR_ID, None)
        with self.assertRaises(synth.Refusal):
            synth.fence(synth.STAGING_GUILD_ID, "1546777867978018887", None)
        for url in ("http://two-bot-next-staging.x.workers.dev", "https://two-bot-next-production.x.workers.dev"):
            with self.assertRaises(synth.Refusal):
                synth.fence(synth.STAGING_GUILD_ID, synth.STAGING_CREATOR_ID, url)

    def test_suffix_userinfo_port_and_path_urls_refuse(self):
        evil = (
            "https://two-bot-next-staging.evil.example",
            "https://two-bot-next-staging.example.workers.dev.evil.example",
            "https://two-bot-next-staging@evil.example",
            "https://user@two-bot-next-staging.example.workers.dev",
            "https://@two-bot-next-staging.example.workers.dev",
            "https://:@two-bot-next-staging.example.workers.dev",
            "https://two-bot-next-staging.example.workers.dev:8443",
            "https://two-bot-next-staging.example.workers.dev/ops/metrics",
            "https://two-bot-next-staging.example.workers.dev?x=1",
            "https://two-bot-next-staging.example.workers.dev#x",
        )
        for url in evil:
            with self.subTest(url=url):
                with self.assertRaises(synth.Refusal) as caught:
                    synth.fence(synth.STAGING_GUILD_ID, synth.STAGING_CREATOR_ID, url)
                self.assertEqual(str(caught.exception),
                                 "staging URL must be the two-bot-next-staging workers.dev origin")

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
        # Half the gateway message cap: fragmentation and ping coverage
        # stay, while the cap boundary itself is pinned by the
        # gateway-intake acceptance file.
        message = json.dumps({"op": 0, "t": "GUILD_CREATE",
                              "d": {"x": "y" * (synth.MAX_BODY_BYTES // 2)}}).encode()
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

    def recvall(self, sock, n):
        buf = b""
        while len(buf) < n:
            chunk = sock.recv(n - len(buf))
            if not chunk:
                self.fail("the pong ended early")
            buf += chunk
        return buf

    def test_ping_at_125_bytes_is_ponged_verbatim(self):
        ours, theirs = socket.socketpair()
        try:
            ws = synth.WebSocket.__new__(synth.WebSocket)
            ws.sock, ws.lock = ours, threading.Lock()
            payload = b"p" * 125
            theirs.sendall(self.frame(0x9, payload) + self.frame(0x8, b""))
            self.assertIsNone(ws.recv())
            pong = self.recvall(theirs, 2 + 4 + 125)
            self.assertEqual(pong[0], 0x8A)
            self.assertEqual(pong[1], 0x80 | 125)
            mask = pong[2:6]
            body = bytes(b ^ mask[i % 4] for i, b in enumerate(pong[6:]))
            self.assertEqual(body, payload)
        finally:
            ours.close()
            theirs.close()

    def test_ping_over_125_bytes_refuses_with_fixed_message(self):
        for size in (126, 1000):
            with self.subTest(size=size):
                ours, theirs = socket.socketpair()
                # Fail fast if the bound is ever removed: without the
                # refusal the reader would block past the pong instead.
                ours.settimeout(5)
                try:
                    ws = synth.WebSocket.__new__(synth.WebSocket)
                    ws.sock, ws.lock = ours, threading.Lock()
                    theirs.sendall(self.frame(0x9, b"q" * size))
                    with self.assertRaises(synth.GatewayShapeError) as ctx:
                        ws.recv()
                    self.assertEqual(str(ctx.exception), "gateway frame was malformed")
                finally:
                    ours.close()
                    theirs.close()

    def test_close_over_125_bytes_refuses_with_fixed_message(self):
        ours, theirs = socket.socketpair()
        try:
            ws = synth.WebSocket.__new__(synth.WebSocket)
            ws.sock, ws.lock = ours, threading.Lock()
            theirs.sendall(self.frame(0x8, b"c" * 126))
            with self.assertRaises(synth.GatewayShapeError) as ctx:
                ws.recv()
            self.assertEqual(str(ctx.exception), "gateway frame was malformed")
        finally:
            ours.close()
            theirs.close()

    def test_continuation_over_cap_refuses_before_payload_read(self):
        cap = synth.MAX_BODY_BYTES
        first = b"j" * (cap - 100)
        # Over-cap continuation header declaring 200 bytes, payload never sent:
        # the pre-read check must refuse without requesting those bytes.
        data = (self.frame(0x1, first, fin=False)
                + bytes([0x80, 126]) + struct.pack(">H", 200))
        ws = synth.WebSocket.__new__(synth.WebSocket)
        sock = _RecordingSocket(data)
        ws.sock, ws.lock = sock, threading.Lock()
        with self.assertRaises(synth.BodyTooLarge) as ctx:
            ws.recv()
        self.assertEqual(str(ctx.exception),
                         "gateway message answered over the byte cap")
        self.assertEqual(sock.requested, [2, 2, cap - 100, 2, 2])

    def test_continuation_exact_cap_passes(self):
        cap = synth.MAX_BODY_BYTES
        payload = json.dumps({"op": 0, "t": "READY",
                              "d": {"user": {"id": "900"}}}).encode()
        payload += b" " * (cap - len(payload))
        self.assertEqual(len(payload), cap)
        first, rest = payload[:-100], payload[-100:]
        data = self.frame(0x1, first, fin=False) + self.frame(0x0, rest)
        ws = synth.WebSocket.__new__(synth.WebSocket)
        sock = _RecordingSocket(data)
        ws.sock, ws.lock = sock, threading.Lock()
        self.assertEqual(ws.recv(), json.loads(payload))


class _HandshakeScript:
    """Scripted socket for the websocket handshake: bytes, then EOF."""

    def __init__(self, data):
        self._buf = bytearray(data)
        self.sent = []

    def recv(self, n):
        chunk = bytes(self._buf[:n])
        del self._buf[:n]
        return chunk

    def sendall(self, data):
        self.sent.append(bytes(data))

    def close(self):
        pass


class _RecordingSocket:
    """Scripted socket logging every requested read size; EOF when exhausted."""

    def __init__(self, data):
        self._buf = bytearray(data)
        self.requested = []
        self.sent = []

    def recv(self, n):
        self.requested.append(n)
        chunk = bytes(self._buf[:n])
        del self._buf[:n]
        return chunk

    def sendall(self, data):
        self.sent.append(bytes(data))

    def close(self):
        pass


class WebSocketHandshakeTest(unittest.TestCase):
    HANDSHAKE_MSG = "gateway handshake answered over the byte cap"

    def connect(self, data):
        script = _HandshakeScript(data)
        context = mock.Mock()
        context.wrap_socket.return_value = script
        with mock.patch.object(synth.socket, "create_connection",
                               return_value=object()), \
                mock.patch.object(synth.ssl, "create_default_context",
                                  return_value=context):
            return synth.WebSocket("gateway.example", "/?v=10&encoding=json")

    def test_valid_handshake_passes(self):
        ws = self.connect(b"HTTP/1.1 101 Switching Protocols\r\n"
                          b"Upgrade: websocket\r\n\r\n")
        self.assertTrue(ws.sock.sent[0].startswith(
            b"GET /?v=10&encoding=json HTTP/1.1\r\n"))
        ws.close()

    def test_handshake_over_cap_refuses_with_fixed_message(self):
        sentinel = "SENTINEL_HANDSHAKE_must_not_echo"
        head = (sentinel + "\r\n").encode() + b"X" * synth.MAX_BODY_BYTES
        self.assertNotIn(b"\r\n\r\n", head)
        with self.assertRaises(synth.BodyTooLarge) as ctx:
            self.connect(head)
        self.assertEqual(str(ctx.exception), self.HANDSHAKE_MSG)
        self.assertNotIn(sentinel, str(ctx.exception))

    def test_handshake_exact_cap_passes(self):
        cap = synth.MAX_BODY_BYTES
        status = b"HTTP/1.1 101 Switching Protocols\r\n"
        head = status + b"X" * (cap - len(status) - 4) + b"\r\n\r\n"
        self.assertEqual(len(head), cap)
        self.assertNotIn(b"\r\n\r\n", head[:-4])
        ws = self.connect(head)
        self.assertTrue(ws.sock.sent[0].startswith(
            b"GET /?v=10&encoding=json HTTP/1.1\r\n"))
        ws.close()

    def test_handshake_cap_plus_one_refuses_with_fixed_message(self):
        cap = synth.MAX_BODY_BYTES
        status = b"HTTP/1.1 101 Switching Protocols\r\n"
        head = status + b"X" * (cap + 1 - len(status) - 4) + b"\r\n\r\n"
        self.assertEqual(len(head), cap + 1)
        self.assertNotIn(b"\r\n\r\n", head[:-4])
        with self.assertRaises(synth.BodyTooLarge) as ctx:
            self.connect(head)
        self.assertEqual(str(ctx.exception), self.HANDSHAKE_MSG)

    def test_handshake_eof_refuses_without_hanging(self):
        with self.assertRaises(ConnectionError) as ctx:
            self.connect(b"HTTP/1.1 101 Switching Protoco")
        self.assertEqual(str(ctx.exception), "gateway closed during handshake")


class _ScriptSock:
    """Socket stand-in: pump only sets timeouts on it."""

    def settimeout(self, timeout):
        pass


class _ScriptGateway:
    """Scripted ``WebSocket`` stand-in returning parsed messages."""

    def __init__(self, script):
        self._script = list(script)
        self.sent = []
        self.sock = _ScriptSock()

    def recv(self):
        assert self._script, "test bug: gateway script exhausted"
        return self._script.pop(0)

    def send(self, payload):
        self.sent.append(payload)


class GatewayDispatchShapeTest(unittest.TestCase):
    DISPATCH_MSG = "gateway dispatch was malformed"

    def pump_session(self, script):
        session = synth.Session.__new__(synth.Session)
        session.ws = _ScriptGateway(script)
        session.seq, session.events, session.me = None, [], None
        return session

    def dispatch_fails(self, message):
        session = self.pump_session([message])
        with self.assertRaises(synth.GatewayShapeError) as ctx:
            session.pump(lambda event: False, time.monotonic() + 5)
        self.assertEqual(str(ctx.exception), self.DISPATCH_MSG)

    def test_non_dict_message_refuses(self):
        self.dispatch_fails([])

    def test_non_string_t_or_non_dict_d_refuses(self):
        for bad in ({"op": 0, "t": 5, "d": {}},
                    {"op": 0, "t": None, "d": {}},
                    {"op": 0, "t": "READY", "d": ["user"]}):
            with self.subTest(message=bad):
                self.dispatch_fails(bad)

    def test_ready_with_non_string_user_id_refuses(self):
        for bad_id in (None, 900, {"snow": "flake"}, ["900"]):
            with self.subTest(id=bad_id):
                self.dispatch_fails({"op": 0, "s": 1, "t": "READY",
                                     "d": {"user": {"id": bad_id}}})
        self.dispatch_fails({"op": 0, "s": 1, "t": "READY", "d": {"user": None}})

    def test_ready_with_string_user_id_passes(self):
        session = self.pump_session([{"op": 0, "s": 1, "t": "READY",
                                      "d": {"user": {"id": "900"}}}])
        event = session.pump(lambda e: True, time.monotonic() + 5)
        self.assertEqual(event, {"t": "READY", "d": {"user": {"id": "900"}}})
        self.assertEqual(session.me, "900")

    def test_guild_create_or_channel_delete_without_string_id_refuses(self):
        for t in ("GUILD_CREATE", "CHANNEL_DELETE"):
            for d in ({}, {"id": None}, {"id": 5001}):
                with self.subTest(t=t, d=d):
                    self.dispatch_fails({"op": 0, "s": 3, "t": t, "d": d})

    def test_guild_create_and_channel_delete_with_string_id_pass(self):
        create = {"op": 0, "s": 3, "t": "GUILD_CREATE", "d": {"id": "111"}}
        delete = {"op": 0, "s": 4, "t": "CHANNEL_DELETE", "d": {"id": "5001"}}
        session = self.pump_session([create, delete])
        # The run_live consumers index e["d"]["id"] directly; the pump
        # validation above is what keeps that indexing safe.
        event = session.pump(
            lambda e: e["t"] == "GUILD_CREATE" and e["d"]["id"] == "111",
            time.monotonic() + 5)
        self.assertEqual(event, {"t": "GUILD_CREATE", "d": {"id": "111"}})
        event = session.pump(
            lambda e: e["t"] == "CHANNEL_DELETE" and e["d"]["id"] == "5001",
            time.monotonic() + 5)
        self.assertEqual(event, {"t": "CHANNEL_DELETE", "d": {"id": "5001"}})


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
        with mock.patch.object(synth, "_open_no_redirect",
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
        with mock.patch.object(synth, "_open_no_redirect",
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
        with mock.patch.object(synth, "_open_no_redirect",
                               return_value=self.response(prefix + b"X", seen)):
            with self.assertRaises(synth.BodyTooLarge) as ctx:
                synth.rest("t", f"/channels/{ROOM}")
        self.assertEqual(str(ctx.exception), "Discord REST read answered over the body cap")
        self.assertEqual(seen, [synth.MAX_BODY_BYTES + 1])

    def test_rest_http_error_path_is_bounded(self):
        seen = []
        big = b"y" * (synth.MAX_BODY_BYTES + 1)
        with mock.patch.object(synth, "_open_no_redirect",
                               side_effect=self.http_error(big, seen)):
            with self.assertRaises(synth.BodyTooLarge) as ctx:
                synth.rest("t", f"/channels/{ROOM}")
        self.assertEqual(str(ctx.exception), "Discord REST read answered over the body cap")
        self.assertEqual(seen, [synth.MAX_BODY_BYTES + 1])
        seen.clear()
        with mock.patch.object(synth, "_open_no_redirect",
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
        with mock.patch.object(synth, "_open_no_redirect",
                               return_value=self.response(body, seen)):
            self.assertEqual(synth.metrics_value("https://two-bot-next-staging.x", "tok"), 4)
        self.assertEqual(seen, [synth.MAX_BODY_BYTES + 1])

    def test_metrics_over_cap_refuses_instead_of_silent_skip(self):
        sentinel = "SENTINEL_BODY_voice_metrics_padded"
        base = (f'# {sentinel}\ntwo_bot_voice_names_total{{outcome="created_with_template"}} 9\n').encode()
        body = base + b" " * (synth.MAX_BODY_BYTES + 1 - len(base))
        self.assertEqual(len(body), synth.MAX_BODY_BYTES + 1)
        seen = []
        with mock.patch.object(synth, "_open_no_redirect",
                               return_value=self.response(body, seen)):
            with self.assertRaises(synth.BodyTooLarge) as ctx:
                synth.metrics_value("https://two-bot-next-staging.x", "tok")
        self.assertEqual(str(ctx.exception), "metrics read answered over the body cap")
        self.assertNotIn(sentinel, str(ctx.exception))
        self.assertEqual(seen, [synth.MAX_BODY_BYTES + 1])

    def test_metrics_http_error_path_is_bounded(self):
        seen = []
        big = b"z" * (synth.MAX_BODY_BYTES + 1)
        with mock.patch.object(synth, "_open_no_redirect",
                               side_effect=self.http_error(big, seen)):
            with self.assertRaises(synth.BodyTooLarge):
                synth.metrics_value("https://two-bot-next-staging.x", "tok")
        self.assertEqual(seen, [synth.MAX_BODY_BYTES + 1])
        seen.clear()
        with mock.patch.object(synth, "_open_no_redirect",
                               side_effect=self.http_error(b"oops", seen)):
            self.assertIsNone(synth.metrics_value("https://two-bot-next-staging.x", "tok"))
        self.assertEqual(seen, [synth.MAX_BODY_BYTES + 1])

    def test_patch_reports_status_without_raising(self):
        seen = []
        body = json.dumps({"id": CREATOR, "name": "Create a Lobby"}).encode()
        with mock.patch.object(synth, "_open_no_redirect",
                               return_value=self.response(body, seen)):
            self.assertEqual(synth.rest_patch("t", f"/channels/{CREATOR}", {"name": "x"}),
                             (200, {"id": CREATOR, "name": "Create a Lobby"}))
        self.assertEqual(seen, [synth.MAX_BODY_BYTES + 1])

    def test_patch_forbidden_reports_403(self):
        seen = []
        with mock.patch.object(synth, "_open_no_redirect",
                               side_effect=self.http_error(b"{}", seen, code=403)):
            self.assertEqual(synth.rest_patch("t", f"/channels/{CREATOR}", {"name": "x"}),
                             (403, None))
        self.assertEqual(seen, [synth.MAX_BODY_BYTES + 1])

    def test_patch_over_cap_refuses(self):
        big = b"y" * (synth.MAX_BODY_BYTES + 1)
        with mock.patch.object(synth, "_open_no_redirect",
                               return_value=self.response(big, [])):
            with self.assertRaises(synth.BodyTooLarge) as ctx:
                synth.rest_patch("t", f"/channels/{CREATOR}", {"name": "x"})
        self.assertEqual(str(ctx.exception), "Discord REST read answered over the body cap")

    def test_user_agent_pins_are_unchanged(self):
        requests = []

        def fake_open(req, timeout=None):
            requests.append(req)
            return self.response(b"{}", [])

        with mock.patch.object(synth, "_open_no_redirect", side_effect=fake_open):
            synth.rest("t", f"/channels/{ROOM}")
            synth.rest_patch("t", f"/channels/{ROOM}", {"name": "x"})
            synth.metrics_value("https://two-bot-next-staging.x", "tok")
        self.assertEqual(len(requests), 3)
        for req in requests:
            self.assertEqual(req.get_header("User-agent"), self.EXPECTED_UA)


class HelloIntervalTest(unittest.TestCase):
    HELLO_MSG = "gateway hello was malformed"

    def session_with_interval(self, interval):
        hello = {"op": 10, "d": {"heartbeat_interval": interval}}

        class _Gateway:
            def __init__(self, *args):
                self.sent = []

            def recv(self):
                return hello

            def send(self, payload):
                self.sent.append(payload)

            def close(self):
                pass

        with mock.patch.object(synth, "WebSocket", _Gateway):
            return synth.Session("TOK")

    def test_valid_interval_becomes_seconds(self):
        session = self.session_with_interval(41250)
        try:
            self.assertEqual(session.interval, 41.25)
        finally:
            session.close()

    def test_invalid_intervals_fail_with_fixed_message(self):
        for bad in (0, -41250, True, float("nan"), float("inf"),
                    10 ** 1000, 1e300):
            with self.subTest(interval=bad), \
                    self.assertRaises(synth.GatewayShapeError) as ctx:
                self.session_with_interval(bad)
            self.assertEqual(str(ctx.exception), self.HELLO_MSG)

    def test_timing_cap_boundary_milliseconds(self):
        session = self.session_with_interval(600000)
        try:
            self.assertEqual(session.interval, 600.0)
        finally:
            session.close()
        with self.assertRaises(synth.GatewayShapeError) as ctx:
            self.session_with_interval(600001)
        self.assertEqual(str(ctx.exception), self.HELLO_MSG)


class OwnedRoomRegressionTest(unittest.TestCase):
    """Additional regressions; the paired protected acceptance stays immutable."""

    def run_case(self, batches, *, creator_name=owned.ORIGINAL_NAME,
                 patch_plan=None):
        rooms = {owned.OWN: owned.room_state(owned.OWN),
                 owned.REJOIN: owned.room_state(owned.REJOIN, position=18)}
        before = [owned.creator_state(owned.TEMP_NAME)]
        fake = owned.make_fake(
            rooms=rooms, before=before, after=before + list(rooms.values()),
            creator_name=creator_name, patch_plan=patch_plan)
        session = owned.FakeSession(batches)
        result = owned.run_with_fakes([], session, fake)
        self.assertTrue(session.closed)
        return result, fake

    def test_cleanup_2xx_with_unchanged_limit_requires_manual_restore(self):
        batches = [
            [owned.GUILD_CREATE],
            [owned.channel_create(owned.OWN), owned.voice_update(owned.OWN)],
            [], [], [],
        ]
        (code, _, _, evidence), fake = self.run_case(
            batches, creator_name=owned.RESIDUE_NAME, patch_plan=[
                ("apply_then_raise", ConnectionError("lost probe response")),
                ("status", 204),
            ])
        self.assertEqual(code, 1)
        self.assertEqual(evidence["room_id"], owned.OWN)
        self.assertEqual(owned.verdicts(evidence)["limit"], "FAIL")
        text = owned.detail(evidence, "limit")
        for snippet in (owned.LIMIT_UNCERTAIN, owned.OWN,
                        str(owned.INITIAL_LIMIT), "restore", "manually"):
            self.assertIn(snippet, text)
        self.assertNotIn("re-run", text)
        self.assertEqual(fake.patches, owned.room_round_trip())
        self.assertEqual(fake.rooms[owned.OWN]["user_limit"], owned.PROBED_LIMIT)
        self.assertLessEqual(fake.gets_to(owned.OWN), 8)

    def test_rejoin_create_after_move_is_collected_during_delete_wait(self):
        batches = [
            [owned.GUILD_CREATE],
            [owned.channel_create(owned.OWN), owned.voice_update(owned.OWN)],
            [owned.status_update(owned.OWN)],
            [owned.voice_update(owned.REJOIN)],
            [owned.channel_create(owned.REJOIN), owned.channel_delete(owned.OWN),
             owned.channel_delete(owned.REJOIN)],
        ]
        (code, _, _, evidence), fake = self.run_case(batches)
        self.assertEqual(code, 0)
        self.assertEqual(evidence["room_id"], owned.OWN)
        # The JSON evidence exposes the correlated subject in the detail.
        self.assertIn(owned.REJOIN, owned.detail(evidence, "rejoin"))
        self.assertEqual(owned.verdicts(evidence)["rejoin"], "PASS")
        self.assertEqual(owned.verdicts(evidence)["delete"], "PASS")
        self.assertEqual(fake.patches, [owned.rename_cycle()[0]]
                         + owned.room_round_trip() + [owned.rename_cycle()[1]])

    def test_wrong_category_create_refuses_before_any_patch(self):
        batches = [
            [owned.GUILD_CREATE],
            [owned.channel_create(owned.OWN, parent_id="other"),
             owned.voice_update(owned.OWN)],
            [], [], [],
        ]
        (code, _, _, evidence), fake = self.run_case(
            batches, creator_name=owned.RESIDUE_NAME)
        self.assertEqual(code, 1)
        self.assertIsNone(evidence["room_id"])
        self.assertEqual(owned.verdicts(evidence)["create"], "FAIL")
        self.assertEqual(fake.patches, [])
        self.assertEqual(fake.gets_to(owned.OWN), 0)


if __name__ == "__main__":
    unittest.main()
