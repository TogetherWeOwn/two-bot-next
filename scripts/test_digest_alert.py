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
    def run_main(self, args, env, side_effect=None):
        with patch.dict(os.environ, env, clear=False):
            for key in ("GITHUB_TOKEN", "RUN_ID", "GITHUB_RUN_ID",
                        "GITHUB_REPOSITORY", "GITHUB_SERVER_URL"):
                os.environ.pop(key, None)
            os.environ.update(env)
            with patch.object(alert.incident_issue, "open_incident",
                              return_value=("opened", 7),
                              side_effect=side_effect) as opened:
                code = alert.main(args)
        return code, opened

    def test_opens_incident_issue_on_missing_digest(self):
        code, opened = self.run_main(
            ["--live-sha", SHA, "--prev-tag", "v2026.10.20"],
            {"GITHUB_TOKEN": "fixture", "RUN_ID": "1", "GITHUB_REPOSITORY": "o/r"})
        self.assertEqual(code, 0)
        opened.assert_called_once()
        _env, key, title, body = opened.call_args.args
        self.assertEqual(key, "daily-digest")
        self.assertIn("daily-digest", title)
        self.assertIn("ALERT daily-digest", body)

    def test_unwritable_issue_fails_closed(self):
        code, opened = self.run_main(
            ["--live-sha", SHA], {"RUN_ID": "1"},
            side_effect=alert.incident_issue.IncidentError("incident sink context is missing"))
        self.assertEqual(code, 1)
        opened.assert_called_once()

    def test_bad_sha_refuses(self):
        code, opened = self.run_main(["--live-sha", "short"], {"GITHUB_TOKEN": "fixture"})
        self.assertEqual(code, 1)
        opened.assert_not_called()

    def test_bad_prev_tag_refuses(self):
        for tag in ("v1.2.3", "v2026.1101.01", "v2026.0101.0", "v2026.1101.0-canary.0", "canary/v2026.1101.0"):
            with self.subTest(tag=tag):
                code, opened = self.run_main(
                    ["--live-sha", SHA, "--prev-tag", tag], {"GITHUB_TOKEN": "fixture"})
                self.assertEqual(code, 1)
                opened.assert_not_called()

    def test_calver_prev_tag_alerts(self):
        # After the CalVer switch the predecessor is vYYYY.MDD.P.
        for tag in ("v2026.1101.0", "v2027.105.0", "v2026.1231.12"):
            with self.subTest(tag=tag):
                code, opened = self.run_main(
                    ["--live-sha", SHA, "--prev-tag", tag], {"GITHUB_TOKEN": "fixture"})
                self.assertEqual(code, 0)
                opened.assert_called_once()
                self.assertIn(tag, opened.call_args.args[3])


if __name__ == "__main__":
    unittest.main()
