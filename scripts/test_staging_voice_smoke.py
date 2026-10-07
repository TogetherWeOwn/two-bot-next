"""Offline staging voice-smoke fixtures; stdlib only, no network access."""

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
import staging_voice_smoke as smoke  # noqa: E402

STAGING_GUILD = "1545644954272137297"
LIVE_GUILD = "326474832151838730"
OTHER_GUILD = "9999999999999999999"
STAGING_APP = "1469137636663758888"
OTHER_APP = "1111111111111111111"
TOKEN = "RAW_SECRET_SENTINEL_never_disclose_bot_token"
WORKER_URL = "https://two-bot-next-staging.5150.workers.dev"


def entry(index, name, **overrides):
    base = {"id": f"{3000000000000000000 + index}", "name": name, "type": 1,
            "guild_id": STAGING_GUILD, "version": f"{4000000000000000000 + index}",
            "options": list(smoke.MOCK_OPTIONS.get(name, []))}
    base.update(overrides)
    return base


def command_list(names):
    return [entry(index, name) for index, name in enumerate(names)]


FULL_NAMES = ["rank", *smoke.VOICE_COMMANDS, *smoke.EXTENDED_COMMANDS]


class VoiceSmokeTests(unittest.TestCase):
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

    def routes(self, commands, app=STAGING_APP):
        app_url = f"{smoke.API}/applications/{app}/guilds/{STAGING_GUILD}/commands"
        mapping = {f"{smoke.API}/users/@me": self.body({"id": app, "username": "fixture"}),
                   app_url: self.body(commands)}
        for command in commands:
            mapping[f"{app_url}/{command['id']}"] = self.body(command)
        return mapping

    def run_main(self, argv, routes, token=TOKEN, worker_fetch=None):
        with mock.patch.dict(os.environ, {"DISCORD_STAGING_BOT_TOKEN": token}, clear=True):
            out = io.StringIO()
            with redirect_stdout(out):
                code = smoke.main(argv + ["--evidence", self.evidence],
                                  fetch_fn=self.fetch(routes), worker_fetch=worker_fetch)
        return code, out.getvalue()

    def read_evidence(self):
        with open(self.evidence, encoding="utf-8") as handle:
            return json.load(handle)

    def test_live_guild_refuses_before_any_request(self):
        code, out = self.run_main(["--guild-id", LIVE_GUILD, "--staging-url", ""],
                                  self.routes(command_list(FULL_NAMES)))
        self.assertEqual(code, 2)
        self.assertEqual(self.requested, [])
        self.assertFalse(Path(self.evidence).exists())
        self.assertIn("FAIL guild-fence: refusing: live guild id must never be smoked", out)

    def test_unknown_guild_refuses_before_any_request(self):
        code, out = self.run_main(["--guild-id", OTHER_GUILD, "--staging-url", ""],
                                  self.routes(command_list(FULL_NAMES)))
        self.assertEqual(code, 2)
        self.assertEqual(self.requested, [])
        self.assertIn("not the TWO Staging guild id", out)

    def test_worker_origin_fence_refuses_production_before_any_request(self):
        code, out = self.run_main(
            ["--guild-id", STAGING_GUILD, "--staging-url", "https://two-bot-next.5150.workers.dev"],
            self.routes(command_list(FULL_NAMES)))
        self.assertEqual(code, 2)
        self.assertEqual(self.requested, [])
        self.assertIn("FAIL worker-origin: refusing:", out)

    def test_missing_token_refuses_before_any_request(self):
        code, out = self.run_main(["--guild-id", STAGING_GUILD, "--staging-url", ""],
                                  self.routes(command_list(FULL_NAMES)), token="")
        self.assertEqual(code, 2)
        self.assertEqual(self.requested, [])
        self.assertIn("no staging bot token given", out)

    def test_wrong_application_refuses_after_identity_only(self):
        code, out = self.run_main(["--guild-id", STAGING_GUILD, "--staging-url", ""],
                                  self.routes(command_list(FULL_NAMES), app=OTHER_APP))
        self.assertEqual(code, 1)
        self.assertEqual(self.requested, [f"{smoke.API}/users/@me"])
        self.assertIn("token is not the staging application", out)

    def test_full_registry_passes_all_lifecycle_probes(self):
        code, out = self.run_main(["--guild-id", STAGING_GUILD, "--staging-url", ""],
                                  self.routes(command_list(FULL_NAMES)))
        self.assertEqual(code, 0)
        for name in ("rank", *smoke.VOICE_COMMANDS, *smoke.EXTENDED_COMMANDS):
            self.assertIn(f"PASS {name}:", out)
        for probe in ("creator-create", "join-move", "kick-ballot", "room-cleanup"):
            self.assertIn(f"PASS {probe}:", out)
        receipt = self.read_evidence()
        self.assertEqual(receipt["result"], "pass")
        self.assertEqual(receipt["guild_id"], STAGING_GUILD)
        self.assertFalse(any(c["verdict"] == "fail" for c in receipt["checks"]))

    def test_missing_voice_commands_skip_lifecycle_and_stay_green(self):
        names = ["rank"]
        code, out = self.run_main(["--guild-id", STAGING_GUILD, "--staging-url", ""],
                                  self.routes(command_list(names)))
        self.assertEqual(code, 0)
        self.assertIn("PASS rank:", out)
        for name in smoke.VOICE_COMMANDS:
            self.assertIn(f"SKIP {name}:", out)
        for probe in ("creator-create", "join-move", "kick-ballot", "room-cleanup"):
            self.assertIn(f"SKIP {probe}:", out)
        self.assertIn("awaiting voice command registration", out)
        receipt = self.read_evidence()
        self.assertEqual(receipt["result"], "pass")
        self.assertFalse(any(c["verdict"] == "fail" for c in receipt["checks"]))

    def test_missing_control_fails_even_when_voice_present(self):
        names = [n for n in FULL_NAMES if n != "rank"]
        code, out = self.run_main(["--guild-id", STAGING_GUILD, "--staging-url", ""],
                                  self.routes(command_list(names)))
        self.assertEqual(code, 1)
        self.assertIn("FAIL rank:", out)

    def test_kick_without_member_option_fails_the_ballot_probe(self):
        commands = command_list(FULL_NAMES)
        for command in commands:
            if command["name"] == "kick":
                command["options"] = []
        code, out = self.run_main(["--guild-id", STAGING_GUILD, "--staging-url", ""],
                                  self.routes(commands))
        self.assertEqual(code, 1)
        self.assertIn("FAIL kick-ballot: /kick registered with neither the voice nor the moderation shape", out)

    def test_moderation_shaped_kick_passes_the_ballot_probe(self):
        # Moderation's /kick (required `target`) wins the first-wins merge;
        # the ballot then runs only after an eligible moderation refusal.
        commands = command_list(FULL_NAMES)
        for command in commands:
            if command["name"] == "kick":
                command["options"] = [
                    {"name": "target", "description": "Member to moderate",
                     "type": 6, "required": True},
                    {"name": "reason", "description": "Mandatory audit reason",
                     "type": 3, "required": True, "max_length": 512},
                ]
        code, out = self.run_main(["--guild-id", STAGING_GUILD, "--staging-url", ""],
                                  self.routes(commands))
        self.assertEqual(code, 0)
        self.assertIn("PASS kick:", out)
        self.assertIn("PASS kick-ballot: /kick is moderation's (target option, first-wins);", out)

    def test_present_but_wrong_command_stays_fail(self):
        # A list entry whose resource disagrees is a registration defect,
        # not a pending registration: FAIL, never SKIP.
        commands = command_list(FULL_NAMES)
        routes = self.routes(commands)
        create = next(c for c in commands if c["name"] == "create")
        app_url = f"{smoke.API}/applications/{STAGING_APP}/guilds/{STAGING_GUILD}/commands"
        impostor = dict(create, name="other")
        routes[f"{app_url}/{create['id']}"] = self.body(impostor)
        code, out = self.run_main(["--guild-id", STAGING_GUILD, "--staging-url", ""],
                                  routes)
        self.assertEqual(code, 1)
        self.assertIn("FAIL create: command resource disagrees with the list entry", out)
        self.assertIn("FAIL creator-create: /create is registered but its resource did not read clean", out)
        self.assertIn("FAIL room-cleanup: /create is registered but its resource did not read clean", out)
        self.assertNotIn("awaiting voice command registration; empty-room", out)

    def test_mock_ignores_staging_url(self):
        def worker_fetch(origin):
            raise AssertionError(f"mock mode must not fetch the Worker: {origin}")

        with mock.patch.dict(os.environ, {}, clear=True):
            out = io.StringIO()
            with redirect_stdout(out):
                code = smoke.main(["--mock", "--staging-url", WORKER_URL,
                                   "--evidence", self.evidence],
                                  worker_fetch=worker_fetch)
        self.assertEqual(code, 0)
        self.assertIn("SKIP worker: no staging Worker URL given; registry-only run",
                      out.getvalue())
        receipt = self.read_evidence()
        self.assertEqual(receipt["transport"], smoke.MOCK_TRANSPORT)
        self.assertIsNone(receipt["worker_url"])

    def test_worker_failure_blocks_before_registry(self):
        def worker_fetch(origin):
            self.assertEqual(origin, WORKER_URL)
            return [smoke.Result("worker-readyz", "fail", "GET /readyz refused with error 'x'")], None

        code, out = self.run_main(["--guild-id", STAGING_GUILD, "--staging-url", WORKER_URL],
                                  self.routes(command_list(FULL_NAMES)), worker_fetch=worker_fetch)
        self.assertEqual(code, 1)
        self.assertIn("FAIL worker-readyz:", out)
        # Registry reads never start when the Worker phase fails.
        self.assertEqual(self.requested, [])

    def test_mock_mode_runs_without_a_token(self):
        with mock.patch.dict(os.environ, {}, clear=True):
            out = io.StringIO()
            with redirect_stdout(out):
                code = smoke.main(["--mock", "--staging-url", "",
                                   "--evidence", self.evidence])
        self.assertEqual(code, 0)
        self.assertIn("PASS creator-create:", out.getvalue())
        receipt = self.read_evidence()
        self.assertEqual(receipt["transport"], smoke.MOCK_TRANSPORT)

    def test_evidence_carries_no_secret_or_credential(self):
        code, _ = self.run_main(["--guild-id", STAGING_GUILD, "--staging-url", ""],
                                self.routes(command_list(FULL_NAMES)))
        self.assertEqual(code, 0)
        raw = Path(self.evidence).read_text()
        for token in ("Bot ", "Bearer", "BEGIN ", "DISCORD_STAGING_BOT_TOKEN", TOKEN):
            self.assertNotIn(token, raw)


if __name__ == "__main__":
    unittest.main()
