#!/usr/bin/env python3
"""Acceptance tests: preview staging voice-synthetic mutations with --dry-run.

Protected acceptance file: the implementation change must satisfy these
tests without editing them. Test defects are corrected here, not by
weakening assertions in the build.

Contract under test (``scripts/staging_voice_synthetic.py`` renames the
staging creator and PATCHes room ``user_limit`` with no preview):

* ``python3 scripts/staging_voice_synthetic.py --dry-run`` exits 0 and
  prints a stable plan to stdout naming every mutation it would perform:
  the creator rename (original, marker name, restore) and the ``user_limit``
  PATCH round-trip plus its restore.
* The plan is stable: two invocations print byte-identical stdout.
* Zero mutations: no REST PATCH, no REST GET, no gateway session, no
  urllib/socket call of any kind.
* A dry run needs no bot token (a preview must not need the secret) and
  never echoes one.
* Fail-closed fences still apply: a non-staging guild, a non-staging
  creator or an invalid timing input refuses exactly like the live path.
* Well-formed live behavior passes through byte-identically (controls).

Discord, the gateway and metrics are fully mocked: no token, no sockets.

Run: PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_staging_voice_synthetic_dry_run_acceptance.py' -v
"""

import argparse
import contextlib
import importlib
import io
import os
import sys
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent))
synth = importlib.import_module("staging_voice_synthetic")

ME, CREATOR, ROOM, OTHER, CAT = "900", synth.STAGING_CREATOR_ID, "5001", "5002", "4000"
OWNER_ALLOW = str(synth.OWNER_BITS | (1 << 21))
TOKEN = "dry-run-fake-token-not-a-secret"


def run_synth(argv, env):
    """Call synth.main() under a hermetic env; return (code, stdout, stderr)."""
    stdout, stderr = io.StringIO(), io.StringIO()
    with (
        patch.dict(os.environ, env, clear=True),
        contextlib.redirect_stdout(stdout),
        contextlib.redirect_stderr(stderr),
    ):
        try:
            code = synth.main(argv)
        except SystemExit as exited:
            code = exited.code if exited.code is not None else 0
    if code in (None, True):
        return 0, stdout.getvalue(), stderr.getvalue()
    return code, stdout.getvalue(), stderr.getvalue()


@contextlib.contextmanager
def no_discord():
    """Refuse every Discord/gateway/metrics/network entry point."""
    with (
        patch.object(synth, "rest", side_effect=AssertionError("REST GET during --dry-run")),
        patch.object(synth, "rest_patch", side_effect=AssertionError("REST PATCH during --dry-run")),
        patch.object(synth, "Session", side_effect=AssertionError("gateway session during --dry-run")),
        patch.object(synth, "WebSocket", side_effect=AssertionError("websocket during --dry-run")),
        patch.object(synth, "_open_no_redirect",
                     side_effect=AssertionError("token-bearing request during --dry-run")),
        patch("socket.create_connection", side_effect=AssertionError("socket during --dry-run")),
        patch("urllib.request.build_opener", side_effect=AssertionError("urlopener during --dry-run")),
        patch("urllib.request.urlopen", side_effect=AssertionError("urlopen during --dry-run")),
    ):
        yield


def healthy_obs():
    return {
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


class DryRunPlanTests(unittest.TestCase):
    def test_dry_run_exits_zero_with_stable_plan_and_zero_discord_calls(self):
        with no_discord():
            code1, out1, err1 = run_synth(["--dry-run"], {})
            code2, out2, _ = run_synth(["--dry-run"], {})
        self.assertEqual(code1, 0, f"--dry-run must exit 0; got {code1}: {err1[-300:]!r}")
        self.assertEqual(code2, 0, f"second --dry-run must exit 0; got {code2}")
        self.assertNotEqual(out1.strip(), "", "plan must print something to stdout")
        self.assertEqual(out1, out2, "dry-run plan must be byte-identical across runs")
        lowered = out1.lower()
        self.assertIn("dry", lowered, "plan must say it is a dry run")
        self.assertIn("plan", lowered, "plan must say it is a plan")
        self.assertIn(synth.STAGING_GUILD_ID, out1, "plan must name the staging guild")
        self.assertIn(synth.STAGING_CREATOR_ID, out1, "plan must name the staging creator")
        self.assertIn("(synthetic)", out1, "plan must name the creator rename marker")
        for token in ("rename", "user_limit", "patch", "restor", "creator"):
            self.assertIn(token, lowered, f"plan must name the {token!r} step")

    def test_dry_run_needs_no_token_but_never_echoes_it(self):
        with no_discord():
            code, out, err = run_synth(
                ["--dry-run"], {"TWO_VOICE_SYNTHETIC_BOT_TOKEN": TOKEN})
        self.assertEqual(code, 0, f"--dry-run must exit 0; got {code}: {err[-300:]!r}")
        self.assertNotIn(TOKEN, out, "plan must never echo the bot token")
        with no_discord():
            code, _, err = run_synth(["--dry-run"], {})
        self.assertEqual(code, 0, f"dry-run must not need a token; got {code}: {err[-300:]!r}")

    def test_dry_run_still_refuses_the_live_guild(self):
        with no_discord():
            code, out, err = run_synth(["--dry-run", "--guild", synth.LIVE_GUILD_ID], {})
        self.assertEqual(code, 2, f"live guild must refuse with exit 2, got {code}")
        self.assertIn("REFUSED", out + err, "refusal must use the fixed REFUSED vocabulary")

    def test_dry_run_still_refuses_a_non_staging_creator(self):
        with no_discord():
            code, out, err = run_synth(["--dry-run", "--creator", "123"], {})
        self.assertEqual(code, 2, f"wrong creator must refuse with exit 2, got {code}")
        self.assertIn("REFUSED", out + err, "refusal must use the fixed REFUSED vocabulary")

    def test_dry_run_composes_with_explicit_timing_flags(self):
        with no_discord():
            code, out, err = run_synth(
                ["--dry-run", "--create-within", "30", "--settle", "5"], {})
        self.assertEqual(
            code, 0,
            f"--dry-run must compose with explicit timing flags; got {code}: {err[-300:]!r}",
        )
        self.assertIn("plan", out.lower(), "plan must still print with explicit timing")


class LiveBehaviorControls(unittest.TestCase):
    """Well-formed live behavior passes through byte-identically."""

    def test_parse_defaults_unchanged(self):
        with patch.dict(os.environ, {}, clear=True):
            args = synth.parse_args([])
        self.assertEqual(args.guild, synth.STAGING_GUILD_ID)
        self.assertEqual(args.creator, synth.STAGING_CREATOR_ID)
        self.assertIsNone(args.staging_url)
        self.assertEqual(args.create_within, 20.0)
        self.assertEqual(args.settle, 8.0)
        self.assertEqual(args.grace, 60.0)
        self.assertEqual(args.delete_slack, 20.0)
        self.assertIsNone(args.evidence)
        self.assertIs(getattr(args, "dry_run", False), False,
                      "dry-run must default off when the flag exists")

    def test_fences_unchanged(self):
        synth.fence(synth.STAGING_GUILD_ID, synth.STAGING_CREATOR_ID, None)
        for guild, creator, url in (
            (synth.LIVE_GUILD_ID, synth.STAGING_CREATOR_ID, None),
            ("999", synth.STAGING_CREATOR_ID, None),
            (synth.STAGING_GUILD_ID, "123", None),
            (synth.STAGING_GUILD_ID, synth.STAGING_CREATOR_ID, "https://evil.invalid/"),
            (synth.STAGING_GUILD_ID, synth.STAGING_CREATOR_ID, "http://x.workers.dev/"),
        ):
            with self.assertRaises(synth.Refusal, msg=f"fence({guild!r}, {creator!r}, {url!r})"):
                synth.fence(guild, creator, url)

    def test_timing_bounds_unchanged(self):
        self.assertEqual(synth.timing_seconds("20"), 20.0)
        self.assertEqual(synth.timing_seconds(20), 20.0)
        for bad in ("0", "-1", "601", "nan", "inf", "-inf", "abc", "", None):
            with self.assertRaises(argparse.ArgumentTypeError, msg=f"timing {bad!r} must refuse"):
                synth.timing_seconds(bad)

    def test_healthy_evaluate_still_passes_every_check(self):
        verdicts = {c["check"]: c["verdict"] for c in synth.evaluate(healthy_obs())}
        self.assertEqual(
            verdicts,
            {name: "PASS" for name in (
                "create", "moved", "rejoin", "name", "position", "owner", "status",
                "delete", "metrics", "rename", "limit", "privacy", "name_panel")},
        )

    def test_invalid_timing_still_refused_on_live_and_dry_run_paths(self):
        # Guards the fix shape too: dry-run must validate timing exactly like
        # the live path instead of bypassing it.
        with no_discord():
            code, _, err = run_synth(["--create-within", "0"], {})
        self.assertEqual(code, 2, "bad live timing must refuse with exit 2")
        self.assertIn("timing input must be", err)
        with no_discord():
            code, _, err = run_synth(["--dry-run", "--create-within", "0"], {})
        self.assertEqual(code, 2, "bad dry-run timing must refuse with exit 2")
        self.assertIn("timing input must be", err)

    def test_live_guild_still_refused_before_contact(self):
        with no_discord():
            code, out, err = run_synth(["--guild", synth.LIVE_GUILD_ID], {})
        self.assertEqual(code, 2, "live guild must still refuse with exit 2")
        self.assertIn("REFUSED", out + err)

    def test_missing_token_still_refused(self):
        with no_discord():
            code, out, err = run_synth([], {})
        self.assertEqual(code, 2, "a missing token must still refuse with exit 2")
        self.assertIn("TWO_VOICE_SYNTHETIC_BOT_TOKEN", err)
        self.assertNotIn("Traceback", err, "refusal must not print a traceback")


if __name__ == "__main__":
    unittest.main()
