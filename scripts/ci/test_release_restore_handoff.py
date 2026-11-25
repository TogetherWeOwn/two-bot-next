"""Exercise the restore handoff using synthetic observations and the checked-in inverse."""

from copy import deepcopy
from datetime import datetime, timedelta
from hashlib import sha256
import json
from pathlib import Path
import unittest


ROOT = Path(__file__).resolve().parents[2]
FIXTURE = Path(__file__).with_name("fixtures") / "release_restore_handoff.json"
INVERSE = ROOT / "sql" / "release_schema_inverse.sql"
UTC_FORMAT = "%Y-%m-%dT%H:%M:%SZ"


def utc_time(observation):
    return datetime.strptime(observation["at"], UTC_FORMAT)


class ReleaseRestoreHandoffTests(unittest.TestCase):
    def setUp(self):
        self.handoff = json.loads(FIXTURE.read_text(encoding="utf-8"))
        self.plan = self.handoff["plan"]

    def assert_handoff(self, observed, plan):
        copy_id = plan["copy_id"]
        retained_ledgers = plan["retained_ledger_sha256"]
        self.assertEqual(set(retained_ledgers), {"cutover", "store"}, "retained ledger digest scope")
        for digest in retained_ledgers.values():
            self.assertIsInstance(digest, str, "retained ledger digest")
            self.assertRegex(digest, r"^[0-9a-f]{64}\Z", "retained ledger digest")
        restored = observed["restored"]
        self.assertEqual(restored["backup_id"], plan["backup_id"], "backup lineage")
        self.assertEqual(restored["copy_id"], copy_id, "restored copy")

        sequence = ("restored", "before", "forward", "undo", "after_undo")
        for previous, following in zip(sequence, sequence[1:]):
            self.assertLess(utc_time(observed[previous]), utc_time(observed[following]),
                            "rehearsal order")
        expected_phases = (
            ("before", "forward", "1.0", [], [], None),
            ("forward", "undo", "1.1", [475, 479, 480, 481, 482], [478], plan["fence_0475"]),
            ("after_undo", None, "1.0", [475], [], plan["fence_0475"]),
        )
        for phase, next_phase, contract, cutover, store, fence in expected_phases:
            result = observed[phase]
            self.assertEqual(result["copy_id"], copy_id, f"{phase} evidence target")
            bot = {
                "copy_id": copy_id, "role": "two_bot_runtime", "windows": plan["bot_windows"],
            }
            web = {
                "copy_id": copy_id, "role": "two_web_reader", "contract": contract,
                "upcoming": plan["web_upcoming"],
            }
            if phase == "forward":
                bot["attendance_windows"] = 0
                web["voice_heat"] = {"cells": 168, "sample_days": 0}
            for app, expected in (("bot", bot), ("web", web)):
                reason = f"{phase} {app.capitalize()} readback"
                readback = result.get(app)
                self.assertIsInstance(readback, dict, reason)
                self.assertEqual(readback, {**expected, "at": readback.get("at")}, reason)
                read_at = utc_time(readback)
                self.assertGreaterEqual(read_at, utc_time(result), f"{reason} precedes phase")
                if next_phase is not None:
                    self.assertLess(read_at, utc_time(observed[next_phase]), f"{reason} after phase")
            self.assertEqual(result["cutover_added"], cutover, f"{phase} Cutover ledger")
            self.assertEqual(result["store_added"], store, f"{phase} Store ledger")
            self.assertEqual(result["fence_0475"], fence, f"{phase} 0475 fence")
            self.assertEqual(result["retained_ledger_sha256"], retained_ledgers,
                             f"{phase} retained ledgers")

        undo = observed["undo"]
        self.assertEqual(undo["copy_id"], copy_id, "undo target")
        self.assertEqual(plan["undo_sha256"], sha256(INVERSE.read_bytes()).hexdigest(),
                         "reviewed schema inverse digest")
        self.assertEqual(undo["sha256"], plan["undo_sha256"], "selected schema inverse")
        self.assertEqual(undo["outcome"], "committed", "schema undo outcome")
        cleanup = observed["cleanup"]
        copy = cleanup["copy_readback"]
        self.assertEqual(copy["copy_id"], copy_id, "copy cleanup target")
        self.assertEqual(copy["state"], "absent", "copy absence")
        entries = cleanup["credential_readback"]
        self.assertEqual(entries["copy_id"], copy_id, "credential cleanup target")
        self.assertEqual(entries["entries"], dict.fromkeys(plan["temporary_entries"], "absent"),
                         "temporary entry absence")
        last_app_read = max(utc_time(observed["after_undo"][app]) for app in ("bot", "web"))
        for readback in (copy, entries):
            self.assertLess(last_app_read, utc_time(readback), "cleanup order")
            self.assertEqual(utc_time(readback).date(), utc_time(restored).date(), "same-day cleanup")

    def assert_refuses_change(self, path, value, reason):
        observed = deepcopy(self.handoff)
        parent = observed
        for key in path[:-1]:
            parent = parent[key]
        parent[path[-1]] = value
        with self.subTest(path=path, reason=reason), self.assertRaisesRegex(AssertionError, reason):
            self.assert_handoff(observed, self.plan)

    def test_complete_handoff_matches_reviewed_inverse(self):
        self.assert_handoff(self.handoff, self.plan)

    def test_fresh_restore_rejects_prior_role_and_cleanup_readbacks(self):
        prior = self.handoff
        self.assert_handoff(prior, self.plan)
        current = deepcopy(prior)
        plan = {**self.plan, "backup_id": "fixture-retry-backup", "copy_id": "fixture-retry-restore"}
        current["plan"] = deepcopy(plan)
        current["restored"]["backup_id"] = plan["backup_id"]
        readbacks = [
            (phase, app, f"{phase} {app.capitalize()} readback", "readback precedes phase")
            for phase in ("before", "forward", "after_undo") for app in ("bot", "web")
        ] + [
            ("cleanup", "copy_readback", "copy cleanup target", "cleanup order"),
            ("cleanup", "credential_readback", "credential cleanup target", "cleanup order"),
        ]
        observations = [current[key] for key in ("restored", "before", "forward", "undo", "after_undo")]
        observations.extend(current[section][key] for section, key, _, _ in readbacks)
        for observation in observations:
            observation["copy_id"] = plan["copy_id"]
            observation["at"] = (utc_time(observation) + timedelta(hours=1)).strftime(UTC_FORMAT)

        self.assert_handoff(current, plan)
        with self.assertRaisesRegex(AssertionError, "backup lineage"):
            self.assert_handoff(prior, plan)

        # Keep the other fields current to exercise the copy and time checks independently.
        for section, key, copy_reason, time_reason in readbacks:
            for field, reason in (("copy_id", copy_reason), ("at", time_reason)):
                with self.subTest(section=section, readback=key, stale=field):
                    replay = deepcopy(current)
                    replay[section][key][field] = prior[section][key][field]
                    with self.assertRaisesRegex(AssertionError, reason):
                        self.assert_handoff(replay, plan)

    def test_requires_distinct_app_role_observations_on_the_restored_copy(self):
        self.assert_refuses_change(("restored", "backup_id"), "other-backup", "backup lineage")
        for phase in ("before", "forward", "after_undo"):
            self.assert_refuses_change((phase, "bot"), None, "Bot readback")
            self.assert_refuses_change((phase, "web"), None, "Web readback")
        self.assert_refuses_change(("forward", "web", "role"), "two_bot_runtime", "Web readback")
        self.assert_refuses_change(("forward", "bot", "copy_id"), "other-copy", "Bot readback")
        self.assert_refuses_change(("forward", "web", "voice_heat"), None, "Web readback")
        self.assert_refuses_change(("after_undo", "web", "upcoming"), [0, None], "Web readback")
        for app in ("bot", "web"):
            self.assert_refuses_change(("after_undo", app), self.handoff["before"][app],
                                       f"{app.capitalize()} readback precedes phase")
        self.assert_refuses_change(("forward", "bot", "at"), self.handoff["undo"]["at"],
                                   "forward Bot readback after phase")

    def test_requires_exact_undo_and_post_undo_state(self):
        self.assert_refuses_change(("undo", "sha256"), "0" * 64, "selected schema inverse")
        self.assert_refuses_change(("undo", "outcome"), "commit_unknown", "schema undo outcome")
        self.assert_refuses_change(("after_undo", "cutover_added"), [], "Cutover ledger")
        self.assert_refuses_change(("after_undo", "store_added"), [478], "Store ledger")
        self.assert_refuses_change(("after_undo", "fence_0475"), "0" * 64, "0475 fence")
        for chain in ("cutover", "store"):
            self.assert_refuses_change(("after_undo", "retained_ledger_sha256", chain), "0" * 64,
                                       "retained ledgers")
        self.assert_refuses_change(("after_undo", "web", "contract"), "1.1", "Web readback")
        self.assert_refuses_change(("undo", "at"), self.handoff["after_undo"]["at"], "rehearsal order")

    def test_refuses_missing_retained_digest_across_all_phases(self):
        for chain in ("cutover", "store"):
            observed = deepcopy(self.handoff)
            for stage in ("plan", "before", "forward", "after_undo"):
                observed[stage]["retained_ledger_sha256"][chain] = None
            with self.subTest(chain=chain), self.assertRaisesRegex(AssertionError, "retained ledger digest"):
                self.assert_handoff(observed, observed["plan"])

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
        for readback in ("copy_readback", "credential_readback"):
            self.assert_refuses_change(("cleanup", readback, "at"), "2026-01-15T12:11:15Z",
                                       "cleanup order")
