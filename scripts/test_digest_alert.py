#!/usr/bin/env python3
"""Offline tests for digest_alert.py: missing-digest alert content and refusals.

No network: delivery is stubbed in-process. Run:
PYTHONDONTWRITEBYTECODE=1 python3 scripts/test_digest_alert.py -v
"""

import importlib.util
import os
import sys
import unittest
from pathlib import Path
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    "digest_alert", Path(__file__).with_name("digest_alert.py"))
alert = importlib.util.module_from_spec(spec)
spec.loader.exec_module(alert)

HOOK = "https://discord.com/api/webhooks/123/abcDEF_-123"
SHA = "0123456789abcdef0123456789abcdef01234567"
RUN_LINK = "https://github.com/o/r/actions/runs/1"


class BuildContentTest(unittest.TestCase):
    def test_alert_names_live_sha_and_prev(self):
        content = alert.build_content(SHA, "v2026.10.20", RUN_LINK)
        self.assertIn("two-bot-next ALERT daily-digest", content)
        self.assertIn(SHA[:7], content)
        self.assertIn("v2026.10.20", content)
        self.assertIn(RUN_LINK, content)
        self.assertNotIn(SHA, content, "full SHA never lands in the alert")

    def test_missing_prev_reads_none(self):
        content = alert.build_content(SHA, "", RUN_LINK)
        self.assertIn("prev none", content)

    def test_bad_sha_reads_unknown(self):
        content = alert.build_content("nope", "v2026.10.20", RUN_LINK)
        self.assertIn("unknown", content)


class MainTest(unittest.TestCase):
    def run_main(self, args, env):
        with patch.dict(os.environ, env, clear=False):
            for key in ("SYNTHETIC_ALERT_WEBHOOK_URL", "RUN_ID", "GITHUB_RUN_ID",
                        "GITHUB_REPOSITORY", "GITHUB_SERVER_URL"):
                os.environ.pop(key, None)
            os.environ.update(env)
            with patch.object(alert, "post") as posted:
                code = alert.main(args)
        return code, posted

    def test_delivers_on_missing_digest(self):
        code, posted = self.run_main(
            ["--live-sha", SHA, "--prev-tag", "v2026.10.20"],
            {"SYNTHETIC_ALERT_WEBHOOK_URL": HOOK, "RUN_ID": "1",
             "GITHUB_REPOSITORY": "o/r"})
        self.assertEqual(code, 0)
        posted.assert_called_once()
        url, content = posted.call_args.args
        self.assertEqual(url, HOOK)
        self.assertIn("ALERT daily-digest", content)

    def test_missing_webhook_fails(self):
        code, posted = self.run_main(["--live-sha", SHA], {"RUN_ID": "1"})
        self.assertEqual(code, 1)
        posted.assert_not_called()

    def test_bad_sha_refuses(self):
        code, posted = self.run_main(
            ["--live-sha", "short"],
            {"SYNTHETIC_ALERT_WEBHOOK_URL": HOOK})
        self.assertEqual(code, 1)
        posted.assert_not_called()

    def test_bad_prev_tag_refuses(self):
        code, posted = self.run_main(
            ["--live-sha", SHA, "--prev-tag", "v1.2.3"],
            {"SYNTHETIC_ALERT_WEBHOOK_URL": HOOK})
        self.assertEqual(code, 1)
        posted.assert_not_called()

    def test_non_discord_webhook_refuses(self):
        from synthetic_alert import AlertError
        with patch.dict(os.environ, {"SYNTHETIC_ALERT_WEBHOOK_URL": "https://example.invalid/hook",
                                      "RUN_ID": "1", "GITHUB_REPOSITORY": "o/r"}):
            with patch.object(alert, "post", side_effect=AlertError("refusing")) as posted:
                self.assertEqual(alert.main(["--live-sha", SHA]), 1)
            posted.assert_called_once()


if __name__ == "__main__":
    unittest.main()
