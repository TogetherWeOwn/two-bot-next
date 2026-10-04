"""Offline staging-guild E2E prep fixtures; stdlib only, no network access."""

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
import staging_guild_e2e_prep as prep  # noqa: E402

STAGING_GUILD = "1545644954272137297"
LIVE_GUILD = "326474832151838730"
OTHER_GUILD = "9999999999999999999"
STAGING_APP = "1469137636663758888"
OTHER_APP = "1111111111111111111"
TOKEN = "RAW_SECRET_SENTINEL_never_disclose_bot_token"


class PrepTests(unittest.TestCase):
    def setUp(self):
        # Any real HTTP is a test failure: the skeleton only gets fixture
        # fetches here (it has no network code of its own).
        guard = mock.patch("urllib.request.OpenerDirector.open",
                           side_effect=AssertionError("network access in a unit test"))
        guard.start()
        self.addCleanup(guard.stop)
        self.requested = []
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.evidence = str(Path(tmp.name) / "evidence.json")

    def counting(self, fetch_fn):
        def fetch(url):
            self.requested.append(url)
            return fetch_fn(url)
        return fetch

    def run_main(self, guild, fetch_fn, token=TOKEN):
        env = {"DISCORD_STAGING_BOT_TOKEN": token,
               "DISCORD_STAGING_GUILD_ID": "env-guild-never-used-by-flag-runs"}
        with mock.patch.dict(os.environ, env, clear=True):
            out = io.StringIO()
            with redirect_stdout(out):
                code = prep.main(["--guild-id", guild, "--mock",
                                  "--evidence", self.evidence],
                                 fetch_fn=self.counting(fetch_fn))
        return code, out.getvalue()

    def run_refused(self, guild):
        code, out = self.run_main(guild, prep.fixture_fetch())
        self.assertEqual(code, 2)
        self.assertEqual(self.requested, [])
        self.assertFalse(Path(self.evidence).exists())
        return out

    def test_live_guild_refuses_before_any_request(self):
        out = self.run_refused(LIVE_GUILD)
        self.assertIn("FAIL guild-fence: refusing: live guild id must never be touched", out)

    def test_unknown_guild_refuses_before_any_request(self):
        out = self.run_refused(OTHER_GUILD)
        self.assertIn("FAIL guild-fence: refusing: not the TWO Staging guild id", out)

    def test_missing_guild_refuses_before_any_request(self):
        out = io.StringIO()
        with mock.patch.dict(os.environ, {"DISCORD_STAGING_BOT_TOKEN": TOKEN}, clear=True):
            with redirect_stdout(out):
                code = prep.main(["--guild-id", "", "--mock",
                                  "--evidence", self.evidence],
                                 fetch_fn=self.counting(prep.fixture_fetch()))
        self.assertEqual(code, 2)
        self.assertEqual(self.requested, [])
        self.assertFalse(Path(self.evidence).exists())
        self.assertIn("no staging guild id given", out.getvalue())

    def test_without_mock_there_is_no_transport(self):
        # Mock-only by construction: with no injected fetch the skeleton
        # refuses instead of falling back to any network path.
        out = io.StringIO()
        with mock.patch.dict(os.environ, {}, clear=True):
            with redirect_stdout(out):
                code = prep.main(["--guild-id", STAGING_GUILD,
                                  "--evidence", self.evidence])
        self.assertEqual(code, 1)
        self.assertIn("no transport given (pass --mock for local fixtures)", out.getvalue())
        self.assertFalse(Path(self.evidence).exists())

    def test_mock_run_passes_and_records_shape(self):
        code, out = self.run_main(STAGING_GUILD, prep.fixture_fetch())
        self.assertEqual(code, 0, out)
        for name in ("rank", "leaderboard"):
            self.assertIn(f"PASS {name}:", out)
        self.assertIn("2/2 checks passed (mock-local-fixtures)", out)
        # Each surface gets its own per-command resource read.
        details = [u for u in self.requested if "/commands/" in u]
        self.assertEqual(len(details), 2)
        receipt = json.loads(Path(self.evidence).read_text())
        self.assertEqual(receipt["application_id"], STAGING_APP)
        self.assertEqual(receipt["guild_id"], STAGING_GUILD)
        self.assertEqual(receipt["transport"], "mock-local-fixtures")
        self.assertEqual(receipt["result"], "pass")
        rows = {row["command"]: row for row in receipt["commands"]}
        self.assertEqual(rows["rank"]["version"], "1000000000000000001")
        self.assertEqual(rows["leaderboard"]["command_id"], "2000000000000000005")

    def test_unpublished_surface_is_red_evidence_not_a_crash(self):
        slim = [c for c in prep.fixture_commands() if c["name"] == "rank"]
        code, out = self.run_main(STAGING_GUILD, prep.fixture_fetch(slim))
        self.assertEqual(code, 1)
        self.assertIn("PASS rank:", out)
        self.assertIn("FAIL leaderboard: not registered in the staging guild", out)
        receipt = json.loads(Path(self.evidence).read_text())
        self.assertEqual(receipt["result"], "fail")
        rows = {row["command"]: row for row in receipt["commands"]}
        self.assertEqual(rows["leaderboard"]["verdict"], "fail")
        self.assertIsNone(rows["leaderboard"]["command_id"])

    def test_mismatched_command_resource_fails_that_command(self):
        commands = [dict(c) for c in prep.fixture_commands()]
        routes = {}
        app_url = f"{prep.API}/applications/{STAGING_APP}/guilds/{STAGING_GUILD}/commands"
        routes[f"{prep.API}/users/@me"] = (
            200, json.dumps({"id": STAGING_APP, "username": "fixture"}).encode())
        routes[app_url] = (200, json.dumps(commands).encode())
        for command in commands:
            routes[f"{app_url}/{command['id']}"] = (200, json.dumps(command).encode())
        wrong = dict(commands[1], name="leaderboard-renamed")
        routes[f"{app_url}/{commands[1]['id']}"] = (200, json.dumps(wrong).encode())

        def fetch(url):
            if url not in routes:
                raise AssertionError(f"unexpected offline fixture route: {url}")
            return routes[url]

        code, out = self.run_main(STAGING_GUILD, fetch)
        self.assertEqual(code, 1)
        self.assertIn("FAIL leaderboard: command resource disagrees with the list entry", out)

    def test_wrong_application_refuses_after_identity_only(self):
        routes = {f"{prep.API}/users/@me":
                  (200, json.dumps({"id": OTHER_APP, "username": "fixture"}).encode())}

        def fetch(url):
            if url not in routes:
                raise AssertionError(f"unexpected offline fixture route: {url}")
            return routes[url]

        code, out = self.run_main(STAGING_GUILD, fetch)
        self.assertEqual(code, 1)
        self.assertEqual(self.requested, [f"{prep.API}/users/@me"])
        self.assertIn("transport is not the staging application", out)

    def test_staging_token_in_environment_is_never_read_or_echoed(self):
        code, out = self.run_main(STAGING_GUILD, prep.fixture_fetch())
        self.assertEqual(code, 0, out)
        self.assertNotIn(TOKEN, out)
        self.assertNotIn(TOKEN, Path(self.evidence).read_text())


if __name__ == "__main__":
    unittest.main()
