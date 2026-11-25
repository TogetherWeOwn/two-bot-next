"""Exercise the restore handoff using synthetic observations and the checked-in inverse."""

from copy import deepcopy
from datetime import datetime
from hashlib import sha256
import json
from pathlib import Path
import unittest


ROOT = Path(__file__).resolve().parents[2]
FIXTURE = Path(__file__).with_name("fixtures") / "release_restore_handoff.json"


def utc_time(observation):
    return datetime.strptime(observation["at"], "%Y-%m-%dT%H:%M:%SZ")


class ReleaseRestoreHandoffTests(unittest.TestCase):
    def setUp(self):
        self.handoff = json.loads(FIXTURE.read_text(encoding="utf-8"))
        self.plan = self.handoff["plan"]

    def assert_handoff(self, observed):
        copy_id = self.plan["copy_id"]
        restored = observed["restored"]
        self.assertEqual(restored["backup_id"], self.plan["backup_id"], "backup lineage")
        self.assertEqual(restored["copy_id"], copy_id, "restored copy")

        expected_phases = (
            ("before", "1.0", [], [], None),
            ("forward", "1.1", [475, 479, 480, 481, 482], [478], self.plan["fence_0475"]),
            ("after_undo", "1.0", [475], [], self.plan["fence_0475"]),
        )
        for phase, contract, cutover, store, fence in expected_phases:
            result = observed[phase]
            self.assertEqual(result["copy_id"], copy_id, f"{phase} evidence target")
            bot = {
                "copy_id": copy_id, "role": "two_bot_runtime", "windows": self.plan["bot_windows"],
            }
            web = {
                "copy_id": copy_id, "role": "two_web_reader", "contract": contract,
                "upcoming": self.plan["web_upcoming"],
            }
            if phase == "forward":
                bot["attendance_windows"] = 0
                web["voice_heat"] = {"cells": 168, "sample_days": 0}
            self.assertEqual(result.get("bot"), bot, f"{phase} Bot readback")
            self.assertEqual(result.get("web"), web, f"{phase} Web readback")
            self.assertEqual(result["cutover_added"], cutover, f"{phase} Cutover ledger")
            self.assertEqual(result["store_added"], store, f"{phase} Store ledger")
            self.assertEqual(result["fence_0475"], fence, f"{phase} 0475 fence")

        undo = observed["undo"]
        self.assertEqual(undo["copy_id"], copy_id, "undo target")
        self.assertEqual(undo["sha256"], self.plan["undo_sha256"], "selected schema inverse")
        self.assertEqual(undo["outcome"], "committed", "schema undo outcome")
        phases = ("restored", "before", "forward", "undo", "after_undo")
        for previous, following in zip(phases, phases[1:]):
            self.assertLess(utc_time(observed[previous]), utc_time(observed[following]),
                            "rehearsal order")

        cleanup = observed["cleanup"]
        copy = cleanup["copy_readback"]
        self.assertEqual(copy["copy_id"], copy_id, "copy cleanup target")
        self.assertEqual(copy["state"], "absent", "copy absence")
        entries = cleanup["credential_readback"]
        self.assertEqual(entries["copy_id"], copy_id, "credential cleanup target")
        self.assertEqual(entries["entries"], dict.fromkeys(self.plan["temporary_entries"], "absent"),
                         "temporary entry absence")
        for readback in (copy, entries):
            self.assertLess(utc_time(observed["after_undo"]), utc_time(readback), "cleanup order")
            self.assertEqual(utc_time(readback).date(), utc_time(restored).date(), "same-day cleanup")

    def assert_refuses_change(self, path, value, reason):
        observed = deepcopy(self.handoff)
        parent = observed
        for key in path[:-1]:
            parent = parent[key]
        parent[path[-1]] = value
        with self.subTest(path=path, reason=reason), self.assertRaisesRegex(AssertionError, reason):
            self.assert_handoff(observed)

    def test_complete_handoff_matches_reviewed_inverse(self):
        inverse = ROOT / "sql" / "release_schema_inverse.sql"
        self.assertEqual(self.plan["undo_sha256"], sha256(inverse.read_bytes()).hexdigest())
        self.assert_handoff(self.handoff)

    def test_requires_distinct_app_role_observations_on_the_restored_copy(self):
        self.assert_refuses_change(("restored", "backup_id"), "other-backup", "backup lineage")
        for phase in ("before", "forward", "after_undo"):
            self.assert_refuses_change((phase, "bot"), None, "Bot readback")
            self.assert_refuses_change((phase, "web"), None, "Web readback")
        self.assert_refuses_change(("forward", "web", "role"), "two_bot_runtime", "Web readback")
        self.assert_refuses_change(("forward", "bot", "copy_id"), "other-copy", "Bot readback")
        self.assert_refuses_change(("forward", "web", "voice_heat"), None, "Web readback")
        self.assert_refuses_change(("after_undo", "web", "upcoming"), [0, None], "Web readback")

    def test_requires_exact_undo_and_post_undo_state(self):
        self.assert_refuses_change(("undo", "sha256"), "0" * 64, "selected schema inverse")
        self.assert_refuses_change(("undo", "outcome"), "commit_unknown", "schema undo outcome")
        self.assert_refuses_change(("after_undo", "cutover_added"), [], "Cutover ledger")
        self.assert_refuses_change(("after_undo", "store_added"), [478], "Store ledger")
        self.assert_refuses_change(("after_undo", "fence_0475"), "0" * 64, "0475 fence")
        self.assert_refuses_change(("after_undo", "web", "contract"), "1.1", "Web readback")
        self.assert_refuses_change(("undo", "at"), self.handoff["after_undo"]["at"], "rehearsal order")

    def test_requires_same_day_absence_of_copy_and_all_temporary_entries(self):
        prefix = ("cleanup", "copy_readback")
        self.assert_refuses_change((*prefix, "state"), "delete_accepted", "copy absence")
        self.assert_refuses_change((*prefix, "copy_id"), "other-copy", "copy cleanup target")
        self.assert_refuses_change((*prefix, "at"), "2026-01-16T00:00:00Z", "same-day cleanup")
        prefix = ("cleanup", "credential_readback")
        self.assert_refuses_change((*prefix, "entries"), {}, "temporary entry absence")
        self.assert_refuses_change((*prefix, "entries", "fixture-web-entry"), "present",
                                   "temporary entry absence")
        self.assert_refuses_change((*prefix, "copy_id"), "other-copy", "credential cleanup target")
        self.assert_refuses_change((*prefix, "at"), self.handoff["before"]["at"], "cleanup order")
