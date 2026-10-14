#!/usr/bin/env python3
"""Failing-first acceptance for PR #961 creator-view refresh (TOG-24332).

Pins the staging-voice-synthetic half of `fix(voice): revalidate stale
creator view from the store`: the creator is renamed before the join and
restored after (the `rename` verdict), and renames go through a bounded
PATCH helper (`rest_patch`) that reports HTTP status instead of raising.

Each test fails without the fix and passes with it. Test file only; the
builder implements production code on this branch and never touches this
file.

Run: PYTHONDONTWRITEBYTECODE=1 python3 scripts/test_land_961_creator_view_refresh_acceptance.py -v
"""

import io
import json
import sys
import unittest
import urllib.error
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import staging_voice_synthetic as synth  # noqa: E402


def obs(**over):
    base = {
        "rename_applied": True,
        "rename_restored": True,
        "rename_name": "Create a Lobby (synthetic)",
        "moved_to": "5001",
    }
    base.update(over)
    return base


def verdicts(observation):
    return {c["check"]: c["verdict"] for c in synth.evaluate(observation)}


class RenameVerdictAcceptanceTest(unittest.TestCase):
    def test_rename_check_exists(self):
        self.assertIn("rename", verdicts(obs()))

    def test_rename_marker_names_the_synthetic_suffix(self):
        self.assertEqual(synth.RENAME_MARKER, " (synthetic)")

    def test_rename_passes_when_applied_and_restored(self):
        self.assertEqual(verdicts(obs())["rename"], "PASS")

    def test_rename_skips_when_the_test_bot_may_not_rename(self):
        result = verdicts(
            obs(rename_applied=False, rename_restored=False, rename_forbidden=True)
        )
        self.assertEqual(result["rename"], "SKIP")

    def test_rename_skip_names_the_missing_grant(self):
        [check] = [
            c
            for c in synth.evaluate(
                obs(rename_applied=False, rename_restored=False, rename_forbidden=True)
            )
            if c["check"] == "rename"
        ]
        self.assertIn("Manage Channels", check["detail"])

    def test_rename_fails_when_the_original_name_is_not_restored(self):
        self.assertEqual(verdicts(obs(rename_restored=False))["rename"], "FAIL")

    def test_rename_fails_when_not_applied(self):
        result = verdicts(obs(rename_applied=False, rename_restored=False))
        self.assertEqual(result["rename"], "FAIL")

    def test_rename_residue_fails_with_the_note(self):
        result = verdicts(
            obs(
                rename_applied=False,
                rename_restored=False,
                rename_note="creator already carries synthetic residue",
            )
        )
        self.assertEqual(result["rename"], "FAIL")


class RestPatchAcceptanceTest(unittest.TestCase):
    def test_rest_patch_helper_exists(self):
        self.assertTrue(
            hasattr(synth, "rest_patch"),
            "staging_voice_synthetic.rest_patch must exist",
        )

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

    def http_error(self, payload, code):
        return urllib.error.HTTPError(
            "https://discord.com/api/v10/x", code, "error", {}, io.BytesIO(payload)
        )

    def test_patch_reports_status_without_raising(self):
        seen = []
        body = json.dumps({"id": "200", "name": "Create a Lobby"}).encode()
        with mock.patch.object(
            synth.urllib.request, "urlopen", return_value=self.response(body, seen)
        ):
            self.assertEqual(
                synth.rest_patch("t", "/channels/200", {"name": "x"}),
                (200, {"id": "200", "name": "Create a Lobby"}),
            )
        self.assertEqual(seen, [synth.MAX_BODY_BYTES + 1])

    def test_patch_forbidden_reports_403(self):
        with mock.patch.object(
            synth.urllib.request,
            "urlopen",
            side_effect=self.http_error(b"{}", 403),
        ):
            self.assertEqual(
                synth.rest_patch("t", "/channels/200", {"name": "x"}), (403, None)
            )

    def test_patch_over_cap_refuses(self):
        big = b"y" * (synth.MAX_BODY_BYTES + 1)
        with mock.patch.object(
            synth.urllib.request, "urlopen", return_value=self.response(big, [])
        ):
            with self.assertRaises(synth.BodyTooLarge) as ctx:
                synth.rest_patch("t", "/channels/200", {"name": "x"})
        self.assertEqual(
            str(ctx.exception), "Discord REST read answered over the body cap"
        )

    def test_patch_sends_the_pinned_user_agent(self):
        requests = []

        def fake_open(req, timeout=None):
            requests.append(req)
            return self.response(b"{}", [])

        with mock.patch.object(synth.urllib.request, "urlopen", side_effect=fake_open):
            synth.rest_patch("t", "/channels/200", {"name": "x"})
        self.assertEqual(len(requests), 1)
        self.assertEqual(
            requests[0].get_header("User-agent"), "two-bot-voice-synthetic (staging)"
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
