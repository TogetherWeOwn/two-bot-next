#!/usr/bin/env python3
"""Acceptance tests: cutover freeze drill --dry-run previews the plan.

DR (staging drill dry-run). Offline only: injected fixture transport, no
network, no subprocess, no Discord, no credentials on disk.

Contract under test
-------------------
1. ``cutover_freeze_drill.py --live --dry-run --channel-id <id> --reason
   <r> --confirm-staging --guild-id <staging> --run-id <fixed>`` with a
   staging token in the environment exits 0, prints a stable plan to
   stdout, and performs zero Discord mutations. Stable means
   byte-identical stdout across two runs. The plan names ``dry-run`` and
   ``plan``, the channel id, the staging guild id, and the nine drill
   steps in order.
2. Zero mutations means the transport is never called with ``post_notice``,
   ``patch_slowmode``, ``patch_overwrite``, ``delete_overwrite`` or
   ``delete_notice``. Read ops (``get_channel``, ``get_notice``,
   ``get_commands``) are allowed for pre-mutation validation. Token
   handling for ``--dry-run`` is unspecified (the builder may read the
   baseline or print a static plan); only input-validation parity is
   pinned.
3. Plan/live pre-mutation parity: every invalid input class (guild fence,
   reason, missing ``--channel-id``, missing ``--confirm-staging``)
   refuses with the identical message in ``--dry-run`` and live mode,
   before any mutation.
4. RED is behavioral: every failing assertion fails on an exit code,
   plan-text or call-count comparison, never on an import error, missing
   fixture or refusal to run. ``--dry-run`` is missing today, so argparse
   exits 2 and the dry-run assertions fail.

Controls assert the existing live/mock refusal vocabulary is unchanged;
they pass before and after the builder's change.

Protected file: the Builder builds on this commit and must never edit
this file; test corrections go back to its author.

Run: PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_cutover_freeze_drill_dryrun_acceptance.py' -v
"""

import contextlib
import io
import os
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import cutover_freeze_drill as drill

STAGING_GUILD = "1545644954272137297"
LIVE_GUILD = "326474832151838730"
OTHER_GUILD = "9999999999999999999"
CHANNEL = "1234567890123456789"
REASON = "cutover freeze rehearsal (staging only)"
RUN_ID = "dry-run-acceptance-1"
TOKEN = "dummy-token-for-offline-doubles"
MUTATING_OPS = ("post_notice", "patch_slowmode", "patch_overwrite",
                "delete_overwrite", "delete_notice")
STEP_ORDER = ("baseline-read", "freeze-notice-post", "slowmode-on",
              "lockdown", "command-surface-verify", "slowmode-restore",
              "unlock-restore", "notice-delete", "restore-verify")


def fixture_double(channel_id=CHANNEL):
    """Counting transport with valid reads for the drill channel."""
    calls = []

    def call(op, **kw):
        calls.append((op, dict(kw)))
        if op == "get_channel":
            return {"id": channel_id, "guild_id": STAGING_GUILD,
                    "name": "cutover-drill", "rate_limit_per_user": 0,
                    "permission_overwrites": []}
        if op == "get_commands":
            return [{"id": "1", "name": "ping"}, {"id": "2", "name": "rank"}]
        if op == "get_notice":
            return {"code": drill.UNKNOWN_MESSAGE_CODE, "message": "Unknown Message"}
        if op in MUTATING_OPS:
            return {"id": "dry-run-never-asserts-this"}
        raise AssertionError(f"unexpected op {op}")

    call.calls = calls
    return call


def run_main(argv, env):
    """Run main offline; argparse exits surface as codes, never errors."""
    with tempfile.TemporaryDirectory() as scratch:
        evidence = str(Path(scratch) / "evidence.json")
        argv = list(argv) + ["--evidence", evidence, "--run-id", RUN_ID]
        out, err = io.StringIO(), io.StringIO()
        with mock.patch.dict(os.environ, env, clear=True), \
                mock.patch("urllib.request.OpenerDirector.open",
                           side_effect=AssertionError("network in a unit test")), \
                contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            try:
                code = drill.main(argv)
            except SystemExit as done:
                code = done.code
        return code, out.getvalue(), err.getvalue()


def run_main_with_transport(argv, env, double):
    with mock.patch.object(drill, "live_transport", return_value=double):
        return run_main(argv, env)


def live_argv(**changes):
    base = {"guild_id": STAGING_GUILD, "channel_id": CHANNEL,
            "reason": REASON, "confirm": True, "live": True}
    base.update(changes)
    argv = ["--guild-id", base["guild_id"], "--reason", base["reason"]]
    if base["live"]:
        argv.append("--live")
    if base.get("channel_id"):
        argv += ["--channel-id", base["channel_id"]]
    if base.get("confirm"):
        argv.append("--confirm-staging")
    return argv


def token_env(**changes):
    env = {"DISCORD_STAGING_BOT_TOKEN": TOKEN,
           "DISCORD_STAGING_GUILD_ID": STAGING_GUILD}
    env.update(changes)
    return env


def mutating(calls):
    return [op for op, _ in calls if op in MUTATING_OPS]


class DryRunPlanTests(unittest.TestCase):
    def setUp(self):
        guard = mock.patch("urllib.request.OpenerDirector.open",
                           side_effect=AssertionError("network in a unit test"))
        guard.start()
        self.addCleanup(guard.stop)

    def dry_run(self):
        double = fixture_double()
        code, out, err = run_main_with_transport(
            [*live_argv(), "--dry-run"], token_env(), double)
        return code, out, err, double

    def test_dry_run_exits_zero_with_stable_plan_and_zero_mutations(self):
        code, out, _err, double = self.dry_run()
        self.assertEqual(code, 0, "a valid --dry-run must exit 0 with a plan")
        self.assertIn("dry-run", out, "the plan must name dry-run")
        self.assertIn("plan", out.lower(), "the plan must name the plan")
        self.assertIn(CHANNEL, out, "the plan must name the channel")
        self.assertIn(STAGING_GUILD, out, "the plan must name the guild")
        positions = []
        for step in STEP_ORDER:
            with self.subTest(step=step):
                self.assertIn(step, out, "the plan must name every step")
                positions.append(out.index(step))
        self.assertEqual(positions, sorted(positions),
                         "the plan steps must appear in drill order")
        self.assertEqual(mutating(double.calls), [],
                         "a plan must never mutate Discord")

    def test_dry_run_plan_is_byte_stable_across_runs(self):
        first = self.dry_run()[1]
        second = self.dry_run()[1]
        self.assertEqual(first, second,
                         "the plan must be byte-identical across runs")
        self.assertTrue(first.strip(), "the plan must not be empty")

    def test_dry_run_plan_matches_live_step_surface(self):
        # The live mock drill runs these nine steps; the plan must name them.
        outcome = drill.run_drill(STAGING_GUILD, "mock-drill-channel", REASON,
                                  drill.mock_transport())
        self.assertEqual(outcome["result"], "pass")
        live_steps = [step["name"] for step in outcome["steps"]]
        self.assertEqual(tuple(live_steps), STEP_ORDER)
        code, plan, _, _ = self.dry_run()
        self.assertEqual(code, 0, "a valid --dry-run must exit 0 with a plan")
        for step in live_steps:
            with self.subTest(step=step):
                self.assertIn(step, plan,
                              "the plan must match the live step surface")


class DryRunRefusalParityTests(unittest.TestCase):
    def setUp(self):
        guard = mock.patch("urllib.request.OpenerDirector.open",
                           side_effect=AssertionError("network in a unit test"))
        guard.start()
        self.addCleanup(guard.stop)

    def assert_parity(self, argv, env, snippet, label):
        live_double = fixture_double()
        with mock.patch.object(drill, "live_transport",
                               return_value=live_double):
            live_code, live_out, live_err = run_main(argv, env)
        self.assertEqual(live_code, 2, f"live must refuse {label}")
        self.assertIn(snippet, live_out + live_err,
                      f"live must name the refusal for {label}")
        self.assertEqual(mutating(live_double.calls), [],
                         f"live refusal must not mutate ({label})")
        dry_double = fixture_double()
        with mock.patch.object(drill, "live_transport",
                               return_value=dry_double):
            dry_code, dry_out, dry_err = run_main([*argv, "--dry-run"], env)
        self.assertEqual(dry_code, 2,
                         f"--dry-run must refuse {label} with exit 2")
        self.assertIn(snippet, dry_out + dry_err,
                      f"--dry-run must name the refusal for {label}")
        self.assertEqual(mutating(dry_double.calls), [],
                         f"--dry-run refusal must not mutate ({label})")

    def test_live_guild_parity(self):
        self.assert_parity(live_argv(guild_id=LIVE_GUILD), token_env(),
                           "live guild", "live guild")

    def test_unknown_guild_parity(self):
        self.assert_parity(live_argv(guild_id=OTHER_GUILD), token_env(),
                           "not the TWO Staging guild", "unknown guild")

    def test_missing_guild_parity(self):
        env = {"DISCORD_STAGING_BOT_TOKEN": TOKEN}
        argv = ["--live", "--channel-id", CHANNEL, "--reason", REASON,
               "--confirm-staging", "--guild-id", ""]
        self.assert_parity(argv, env, "no staging guild id", "missing guild")

    def test_blank_reason_parity(self):
        self.assert_parity(live_argv(reason="   "), token_env(),
                           "non-blank audit reason", "blank reason")

    def test_long_reason_parity(self):
        self.assert_parity(live_argv(reason="x" * 513), token_env(),
                           "exceeds 512", "long reason")

    def test_missing_channel_id_parity(self):
        argv = live_argv(channel_id=None)
        self.assert_parity(argv, token_env(),
                           "without --channel-id", "missing channel")

    def test_missing_confirm_parity(self):
        argv = live_argv(confirm=False)
        self.assert_parity(argv, token_env(),
                           "without --confirm-staging", "missing confirm")


class LiveRefusalControls(unittest.TestCase):
    """Existing live/mock vocabulary; green before and after the change."""

    def setUp(self):
        guard = mock.patch("urllib.request.OpenerDirector.open",
                           side_effect=AssertionError("network in a unit test"))
        guard.start()
        self.addCleanup(guard.stop)
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.evidence = str(Path(tmp.name) / "evidence.json")

    def run_live(self, argv, env):
        argv = list(argv) + ["--evidence", self.evidence, "--run-id", RUN_ID]
        out = io.StringIO()
        with mock.patch.dict(os.environ, env, clear=True), redirect_stdout(out):
            try:
                code = drill.main(argv)
            except SystemExit as done:
                code = done.code
        return code, out.getvalue()

    def test_mock_still_passes(self):
        code, _ = self.run_live(["--mock", "--guild-id", STAGING_GUILD],
                                token_env())
        self.assertEqual(code, 0)

    def test_live_guards_unchanged(self):
        code, out = self.run_live(live_argv(channel_id=None), token_env())
        self.assertEqual(code, 2)
        self.assertIn("without --channel-id", out)
        code, out = self.run_live(live_argv(confirm=False), token_env())
        self.assertEqual(code, 2)
        self.assertIn("without --confirm-staging", out)
        env = {"DISCORD_STAGING_GUILD_ID": STAGING_GUILD}
        code, out = self.run_live(live_argv(), env)
        self.assertEqual(code, 2)
        self.assertIn("no staging bot token", out)

    def test_guild_fence_unchanged(self):
        for guild, snippet in ((LIVE_GUILD, "live guild"),
                               (OTHER_GUILD, "not the TWO Staging guild")):
            with self.subTest(guild=guild):
                code, out = self.run_live(live_argv(guild_id=guild), token_env())
                self.assertEqual(code, 2)
                self.assertIn(snippet, out)

    def test_reason_fence_unchanged(self):
        for reason, snippet in (("   ", "non-blank audit reason"),
                                ("x" * 513, "exceeds 512")):
            with self.subTest(snippet=snippet):
                code, out = self.run_live(live_argv(reason=reason), token_env())
                self.assertEqual(code, 2)
                self.assertIn(snippet, out)


if __name__ == "__main__":
    unittest.main()
