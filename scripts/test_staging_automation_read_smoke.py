"""Offline automation-read-smoke fixtures; stdlib only, no network access."""

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
import staging_automation_read_smoke as smoke  # noqa: E402

STAGING_GUILD = "1545644954272137297"
LIVE_GUILD = "326474832151838730"
OTHER_GUILD = "9999999999999999999"
STAGING_APP = "1469137636663758888"
OTHER_APP = "1111111111111111111"
TOKEN = "RAW_SECRET_SENTINEL_never_disclose_bot_token"

COMMANDS = [
    {"id": "1549621908675366953", "name": "rank", "type": 1,
     "guild_id": STAGING_GUILD, "version": "1000000000000000001"},
    {"id": "2000000000000000001", "name": "command-list", "type": 1,
     "guild_id": STAGING_GUILD, "version": "1000000000000000002"},
    {"id": "2000000000000000002", "name": "schedule-list", "type": 1,
     "guild_id": STAGING_GUILD, "version": "1000000000000000003"},
    {"id": "2000000000000000003", "name": "feed-list", "type": 1,
     "guild_id": STAGING_GUILD, "version": "1000000000000000004"},
]


class AutomationReadSmokeTests(unittest.TestCase):
    def setUp(self):
        # Any real HTTP is a test failure: the smoke only gets fake fetches here.
        guard = mock.patch.object(smoke.urllib.request.OpenerDirector, "open",
                                  side_effect=AssertionError("network access in a unit test"))
        guard.start()
        self.addCleanup(guard.stop)
        self.requested = []
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.evidence = str(Path(tmp.name) / "evidence.json")

    def fetch(self, routes):
        def fake(url):
            self.requested.append(url)
            if url not in routes:
                raise AssertionError(f"unexpected offline fixture route: {url}")
            response = routes[url]
            if isinstance(response, Exception):
                raise response
            return response
        return fake

    def body(self, value):
        return 200, json.dumps(value).encode()

    def routes(self, commands=COMMANDS, app=STAGING_APP):
        app_url = f"{smoke.API}/applications/{app}/guilds/{STAGING_GUILD}/commands"
        mapping = {f"{smoke.API}/users/@me": self.body({"id": app, "username": "fixture"}),
                   app_url: self.body(commands)}
        for command in commands:
            mapping[f"{app_url}/{command['id']}"] = self.body(command)
        return mapping

    def run_main(self, guild, routes, token=TOKEN):
        with mock.patch.dict(os.environ, {"DISCORD_STAGING_BOT_TOKEN": token}, clear=True):
            out = io.StringIO()
            with redirect_stdout(out):
                code = smoke.main(["--guild-id", guild, "--evidence", self.evidence],
                                  fetch_fn=self.fetch(routes))
        return code, out.getvalue()

    def run_refused(self, guild, token=TOKEN):
        routes = self.routes()
        code, out = self.run_main(guild, routes, token)
        self.assertEqual(code, 2)
        self.assertEqual(self.requested, [])
        self.assertFalse(Path(self.evidence).exists())
        return out

    def test_live_guild_refuses_before_any_request(self):
        out = self.run_refused(LIVE_GUILD)
        self.assertIn("FAIL guild-fence: refusing: live guild id must never be smoked", out)

    def test_unknown_guild_refuses_before_any_request(self):
        out = self.run_refused(OTHER_GUILD)
        self.assertIn("FAIL guild-fence: refusing: not the TWO Staging guild id", out)

    def test_missing_guild_refuses_before_any_request(self):
        with mock.patch.dict(os.environ, {"DISCORD_STAGING_BOT_TOKEN": TOKEN}, clear=True):
            out = io.StringIO()
            with redirect_stdout(out):
                code = smoke.main(["--guild-id", "", "--evidence", self.evidence],
                                  fetch_fn=self.fetch(self.routes()))
        self.assertEqual(code, 2)
        self.assertEqual(self.requested, [])
        self.assertIn("no staging guild id given", out.getvalue())

    def test_missing_token_refuses_before_any_request(self):
        out = self.run_refused(STAGING_GUILD, token="")
        self.assertIn("no staging bot token given", out)

    def test_wrong_application_refuses_after_identity_only(self):
        code, out = self.run_main(STAGING_GUILD, self.routes(app=OTHER_APP))
        self.assertEqual(code, 1)
        self.assertEqual(self.requested, [f"{smoke.API}/users/@me"])
        self.assertIn("token is not the staging application", out)

    def test_all_registered_passes_and_records_versions(self):
        code, out = self.run_main(STAGING_GUILD, self.routes())
        self.assertEqual(code, 0)
        for name in ("rank", "command-list", "schedule-list", "feed-list"):
            self.assertIn(f"PASS {name}:", out)
        # Every surface gets its own per-command resource read.
        details = [u for u in self.requested if "/commands/" in u]
        self.assertEqual(len(details), 4)
        receipt = json.loads(Path(self.evidence).read_text())
        self.assertEqual(receipt["application_id"], STAGING_APP)
        self.assertEqual(receipt["guild_id"], STAGING_GUILD)
        self.assertEqual(receipt["result"], "pass")
        rows = {row["command"]: row for row in receipt["commands"]}
        self.assertEqual(rows["command-list"]["version"], "1000000000000000002")
        self.assertEqual(rows["feed-list"]["command_id"], "2000000000000000003")

    def test_unpublished_automation_is_red_evidence_not_a_crash(self):
        slim = [c for c in COMMANDS if c["name"] == "rank"]
        code, out = self.run_main(STAGING_GUILD, self.routes(commands=slim))
        self.assertEqual(code, 1)
        self.assertIn("PASS rank:", out)
        for name in ("command-list", "schedule-list", "feed-list"):
            self.assertIn(f"FAIL {name}: not registered in the staging guild", out)
        receipt = json.loads(Path(self.evidence).read_text())
        self.assertEqual(receipt["result"], "fail")
        rows = {row["command"]: row for row in receipt["commands"]}
        self.assertEqual(rows["schedule-list"]["verdict"], "fail")
        self.assertIsNone(rows["schedule-list"]["command_id"])

    def test_mismatched_command_resource_fails_that_command(self):
        commands = [dict(COMMANDS[0]), dict(COMMANDS[1])]
        routes = self.routes(commands=commands)
        app_url = f"{smoke.API}/applications/{STAGING_APP}/guilds/{STAGING_GUILD}/commands"
        wrong = dict(commands[1], name="command-list-renamed")
        routes[f"{app_url}/{commands[1]['id']}"] = self.body(wrong)
        code, out = self.run_main(STAGING_GUILD, routes)
        self.assertEqual(code, 1)
        self.assertIn("FAIL command-list: command resource disagrees with the list entry", out)

    def test_unauthorized_is_a_fixed_refusal_without_retry(self):
        routes = {f"{smoke.API}/users/@me": (401, b"")}
        code, out = self.run_main(STAGING_GUILD, routes)
        self.assertEqual(code, 1)
        self.assertEqual(self.requested, [f"{smoke.API}/users/@me"])
        self.assertIn("credential refused (401); rotation is an operator decision", out)

    def test_rate_limit_fails_without_hammering(self):
        routes = {f"{smoke.API}/users/@me": (429, b"")}
        code, out = self.run_main(STAGING_GUILD, routes)
        self.assertEqual(code, 1)
        self.assertEqual(len(self.requested), 1)
        self.assertIn("rate limited (429); rerun later", out)

    def test_token_never_reaches_stdout_or_evidence(self):
        code, out = self.run_main(STAGING_GUILD, self.routes())
        self.assertEqual(code, 0)
        self.assertNotIn(TOKEN, out)
        self.assertNotIn(TOKEN, Path(self.evidence).read_text())


if __name__ == "__main__":
    unittest.main()
