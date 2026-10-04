"""Offline cutover freeze-drill harness checks; stdlib only, no network access."""

import io
import json
import os
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import cutover_freeze_drill as drill  # noqa: E402

STAGING_GUILD = "1545644954272137297"
LIVE_GUILD = "326474832151838730"
OTHER_GUILD = "9999999999999999999"
TOKEN = "RAW_SECRET_SENTINEL_never_disclose_bot_token"
REASON = "cutover freeze rehearsal (staging only)"


class DrillTests(unittest.TestCase):
    def setUp(self):
        # Any real HTTP is a test failure: the harness only gets the mock
        # transport or an injected double here, never live network.
        guard = mock.patch("urllib.request.OpenerDirector.open",
                           side_effect=AssertionError("network access in a unit test"))
        guard.start()
        self.addCleanup(guard.stop)
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.evidence = str(Path(tmp.name) / "evidence.json")

    def run_mock_main(self, *argv):
        env = {"DISCORD_STAGING_BOT_TOKEN": TOKEN,
               "DISCORD_STAGING_GUILD_ID": "env-guild-never-used-by-flag-runs"}
        with mock.patch.dict(os.environ, env, clear=True):
            out = io.StringIO()
            with redirect_stdout(out):
                code = drill.main(list(argv) + ["--evidence", self.evidence])
        return code, out.getvalue()

    def test_live_guild_refuses_before_any_request(self):
        requested = []
        call = drill.mock_transport()

        def counting(op, **kw):
            requested.append(op)
            return call(op, **kw)

        with self.assertRaises(drill.DrillError) as ctx:
            drill.run_drill(LIVE_GUILD, "chan", REASON, counting)
        self.assertEqual(requested, [])
        self.assertIn("live guild", str(ctx.exception))

    def test_unknown_and_missing_guild_refuse(self):
        for guild in (OTHER_GUILD, ""):
            with self.assertRaises(drill.DrillError, msg=guild or "missing"):
                drill.guild_fence(guild)

    def test_blank_and_long_reason_refuse(self):
        with self.assertRaises(drill.DrillError):
            drill.check_reason("   ")
        with self.assertRaises(drill.DrillError):
            drill.check_reason("x" * 513)
        self.assertEqual(drill.check_reason("  rehearsal  "), "rehearsal")

    def test_lockdown_masks_mirror_core_planner(self):
        # Clears SEND_MESSAGES from allow, sets it in deny, preserves the rest.
        allow, deny = drill.plan_lockdown_masks("4096", "0")
        self.assertEqual((allow, deny), ("4096", "2048"))
        allow, deny = drill.plan_lockdown_masks("6144", "2048")
        self.assertEqual((allow, deny), ("4096", "2048"))
        with self.assertRaises(drill.DrillError):
            drill.plan_lockdown_masks("not-a-mask", "0")

    def test_mock_full_drill_passes_with_timings_and_restore(self):
        call = drill.mock_transport()
        outcome = drill.run_drill(STAGING_GUILD, "mock-drill-channel", REASON, call)
        self.assertEqual(outcome["result"], "pass")
        names = [s["name"] for s in outcome["steps"]]
        self.assertEqual(names, ["baseline-read", "freeze-notice-post", "slowmode-on",
                                 "lockdown", "command-surface-verify", "slowmode-restore",
                                 "unlock-restore", "notice-delete", "restore-verify"])
        for step in outcome["steps"]:
            self.assertEqual(step["result"], "pass")
            self.assertGreaterEqual(step["duration_ms"], 0)
            self.assertRegex(step["started_utc"], r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$")
        restore = outcome["restore"]
        self.assertTrue(restore["restored"])
        self.assertEqual(restore["pre_hash"], restore["post_hash"])
        self.assertTrue(restore["unlocked"])
        self.assertTrue(restore["notice_removed"])
        # The guild is left as found: no notices, no slowmode, no overwrite.
        self.assertEqual(call.state["notices"], {})
        self.assertEqual(call.state["channel"]["rate_limit_per_user"], 0)
        self.assertEqual(call.state["channel"]["permission_overwrites"], [])

    def test_foreign_role_overwrite_is_skipped(self):
        channel = {"id": "chan", "guild_id": STAGING_GUILD,
                   "rate_limit_per_user": 0,
                   "permission_overwrites": [
                       {"id": "5555555555555555555", "type": 0,
                        "allow": "999", "deny": "888"},
                       {"id": STAGING_GUILD, "type": 0,
                        "allow": "4096", "deny": "0"}]}
        self.assertEqual(drill.everyone_overwrite(channel),
                         {"allow": "4096", "deny": "0", "exists": True})

    def test_drill_with_preceding_foreign_overwrite_uses_everyone_seed(self):
        call = drill.mock_transport()
        call.state["channel"]["permission_overwrites"] = [
            {"id": "5555555555555555555", "type": 0,
             "allow": "999", "deny": "888"},
            {"id": STAGING_GUILD, "type": 0,
             "allow": "4096", "deny": "0"}]
        outcome = drill.run_drill(STAGING_GUILD, "mock-drill-channel", REASON, call)
        self.assertEqual(outcome["result"], "pass")
        self.assertTrue(outcome["restore"]["restored"])
        # The foreign overwrite is untouched; @everyone is back to its seed.
        self.assertEqual(call.state["channel"]["permission_overwrites"], [
            {"id": "5555555555555555555", "type": 0,
             "allow": "999", "deny": "888"},
            {"id": STAGING_GUILD, "type": 0,
             "allow": "4096", "deny": "0"}])

    def test_missing_guild_id_refuses_without_mutation(self):
        requested = []
        inner = drill.mock_transport()
        inner.state["channel"].pop("guild_id")

        def counting(op, **kw):
            requested.append(op)
            return inner(op, **kw)

        outcome = drill.run_drill(STAGING_GUILD, "mock-drill-channel", REASON, counting)
        self.assertEqual(outcome["result"], "fail")
        self.assertEqual([s["name"] for s in outcome["steps"] if s["result"] == "fail"],
                         ["baseline-read"])
        self.assertNotIn("post_notice", requested)
        self.assertEqual(inner.state["notices"], {})

    def test_garbage_payload_normalizes_to_drill_error(self):
        inner = drill.mock_transport()
        inner.state["channel"]["rate_limit_per_user"] = "not-a-number"

        def counting(op, **kw):
            return inner(op, **kw)

        outcome = drill.run_drill(STAGING_GUILD, "mock-drill-channel", REASON, counting)
        self.assertEqual(outcome["result"], "fail")
        failed = [s for s in outcome["steps"] if s["result"] == "fail"]
        self.assertEqual([s["name"] for s in failed], ["baseline-read"])
        self.assertIn("unparseable slowmode", failed[0]["detail"])

    def test_missing_ping_surface_fails_and_restores(self):
        call = drill.mock_transport()
        call.state["commands"] = [{"id": "2", "name": "rank"}]
        outcome = drill.run_drill(STAGING_GUILD, "mock-drill-channel", REASON, call)
        self.assertEqual(outcome["result"], "fail")
        failed = [s for s in outcome["steps"] if s["result"] == "fail"]
        self.assertEqual([s["name"] for s in failed], ["command-surface-verify"])
        # Best-effort restore ran: the channel is back to baseline.
        self.assertEqual(call.state["channel"]["rate_limit_per_user"], 0)
        self.assertEqual(call.state["channel"]["permission_overwrites"], [])
        self.assertEqual(call.state["notices"], {})

    def test_cli_mock_run_writes_allowlisted_evidence(self):
        code, out = self.run_mock_main("--mock", "--guild-id", STAGING_GUILD,
                                       "--run-id", "drill-test-001")
        self.assertEqual(code, 0, out)
        self.assertIn("9/9 steps passed (mock-local-fixtures)", out)
        receipt = json.loads(Path(self.evidence).read_text())
        self.assertEqual(receipt["run_id"], "drill-test-001")
        self.assertEqual(receipt["guild_id"], STAGING_GUILD)
        self.assertEqual(receipt["transport"], "mock-local-fixtures")
        self.assertEqual(receipt["result"], "pass")
        self.assertTrue(receipt["restore"]["restored"])
        blob = json.dumps(receipt)
        self.assertNotIn(TOKEN, blob)
        self.assertNotIn("DISCORD_STAGING_BOT_TOKEN", blob)

    def test_cli_live_requires_confirmations(self):
        code, out = self.run_mock_main("--live", "--guild-id", STAGING_GUILD)
        self.assertEqual(code, 2)
        self.assertIn("--confirm-staging", out)

    def test_cli_refuses_live_guild_with_exit_2(self):
        code, out = self.run_mock_main("--mock", "--guild-id", LIVE_GUILD)
        self.assertEqual(code, 2)
        self.assertIn("live guild", out)
        self.assertFalse(Path(self.evidence).exists())


if __name__ == "__main__":
    unittest.main()
