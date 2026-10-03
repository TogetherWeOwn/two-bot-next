"""Offline voice-cutover acceptance fixtures; stdlib only, no network access."""

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
import voice_cutover_staging_acceptance as gate  # noqa: E402

STAGING_GUILD = "1545644954272137297"
LIVE_GUILD = "326474832151838730"
OTHER_GUILD = "9999999999999999999"
STAGING_APP = "1469137636663758888"
OTHER_APP = "1111111111111111111"
TOKEN = "RAW_SECRET_SENTINEL_never_disclose_bot_token"
ORIGIN = "https://two-bot-next-staging.fixture.workers.dev"
HEAD = "87d980608c4f0ea36daf2ff40920e28ee0ee8d6b"

ROUTER_OK = "\n".join(gate.REQUIRED_ROUTER_COPY) + "\n"
REPLIES_OK = "\n".join(gate.REQUIRED_REPLIES_COPY) + "\n"
CONTRACT_OK = (
    '//! | unknown command | none | `This interaction is no longer available.` '
    '([`UNKNOWN_INTERACTION_REPLY`], documented in `docs/interaction-replies.md`) |\n'
    '//! | permission denied | none | `Manage Server permission is required.` '
    '([`RouterRefusal::ManageServerRequired`]) |\n'
    'assert_eq!(content, UNKNOWN_COMMAND_REPLY);\n'
    'assert_eq!(content, RouterRefusal::ManageServerRequired.message());\n'
)


def body(value):
    return 200, {}, json.dumps(value).encode()


def health_ok(revision=HEAD, components=("process", "gateway")):
    return (200, {"x-two-worker-version": "v1"},
            json.dumps({"status": "ok", "build_revision": revision,
                        "build_id": "b1",
                        "components": [[c, "ready"] for c in components]}).encode())


def fenced():
    return 503, {"x-two-worker-version": "v1"}, b'{"error":"ownership_fenced","reason":"not_owner"}'


class AcceptanceTests(unittest.TestCase):
    def setUp(self):
        guard = mock.patch.object(gate.urllib.request.OpenerDirector, "open",
                                  side_effect=AssertionError("network access in a unit test"))
        guard.start()
        self.addCleanup(guard.stop)
        self.requested = []
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.tmp = Path(tmp.name)
        self.evidence = str(self.tmp / "evidence.json")
        self.root = self.tmp / "repo"
        (self.root / "crates" / "core" / "src" / "router").mkdir(parents=True)
        (self.root / "crates" / "bot" / "src").mkdir(parents=True)
        (self.root / "crates" / "core" / "src" / "router.rs").write_text(ROUTER_OK)
        (self.root / "crates" / "core" / "src" / "router" / "replies.rs").write_text(REPLIES_OK)
        (self.root / "crates" / "bot" / "src" / "smoke_error_contract_tests.rs").write_text(CONTRACT_OK)
        (self.root / "crates" / "bot" / "src" / "voice_rooms.rs").write_text(
            "// no stale voice denial literals here\n")

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

    def guild_routes(self, commands=("rank", "create", "setup", "access"),
                     app=STAGING_APP, health=None, readyz=None):
        entries = [{"id": str(1000 + i), "name": name, "type": 1,
                    "guild_id": STAGING_GUILD, "version": "v1"}
                   for i, name in enumerate(commands)]
        app_url = f"{gate.API}/applications/{app}/guilds/{STAGING_GUILD}/commands"
        routes = {f"{gate.API}/users/@me": body({"id": app, "username": "fixture"}),
                  app_url: body(entries)}
        routes[ORIGIN + "/health"] = health or health_ok()
        routes[ORIGIN + "/readyz"] = readyz or health_ok()
        return routes

    def run_main(self, args, routes, token=TOKEN):
        with mock.patch.dict(os.environ, {"DISCORD_STAGING_BOT_TOKEN": token}, clear=True):
            out = io.StringIO()
            with redirect_stdout(out):
                code = gate.main(args + ["--evidence", self.evidence],
                                 fetch_fn=self.fetch(routes), root=self.root)
        return code, out.getvalue()

    def base_args(self, **over):
        args = ["--head-sha", HEAD, "--staging-url", ORIGIN,
                "--guild-id", STAGING_GUILD]
        for key, value in over.items():
            args += [f"--{key.replace('_', '-')}", value]
        return args

    def test_live_guild_refuses_before_any_request(self):
        code, out = self.run_main(self.base_args(guild_id=LIVE_GUILD),
                                  self.guild_routes())
        self.assertEqual(code, 2)
        self.assertEqual(self.requested, [])
        self.assertIn("BLOCKED gate-fence: refusing: live guild", out)
        self.assertFalse(Path(self.evidence).exists())

    def test_unknown_guild_refuses_before_any_request(self):
        code, out = self.run_main(self.base_args(guild_id=OTHER_GUILD),
                                  self.guild_routes())
        self.assertEqual(code, 2)
        self.assertEqual(self.requested, [])
        self.assertIn("not the TWO Staging guild id", out)

    def test_bad_origin_refuses_before_any_request(self):
        code, out = self.run_main(self.base_args(staging_url="https://example.com/"),
                                  self.guild_routes())
        self.assertEqual(code, 2)
        self.assertEqual(self.requested, [])
        self.assertIn("not the two-bot-next-staging workers.dev origin", out)

    def test_bad_sha_refuses_before_any_request(self):
        code, out = self.run_main(
            ["--head-sha", "short", "--staging-url", ORIGIN,
             "--guild-id", STAGING_GUILD],
            self.guild_routes())
        # argparse-level: --head-sha accepts any string; the fence refuses it
        self.assertEqual(code, 2)
        self.assertIn("full 40-hex commit", out)

    def test_fenced_singleton_blocks_health_and_ghosts(self):
        code, out = self.run_main(self.base_args(), self.guild_routes(
            health=fenced(), readyz=fenced()))
        self.assertEqual(code, 1)
        self.assertIn("FAIL P1a staging-health: health 503 ownership_fenced", out)
        self.assertIn("FAIL P1b staging-readyz: readyz 503 ownership_fenced", out)
        self.assertIn("BLOCKED C1 rooms-live-practice", out)
        self.assertIn("BLOCKED C2b ghost-live-poll", out)
        self.assertIn(f"QA {HEAD}: NEEDS WORK", out)

    def test_readyz_revision_mismatch_fails(self):
        code, out = self.run_main(self.base_args(), self.guild_routes(
            readyz=health_ok(revision="0" * 40)))
        self.assertEqual(code, 1)
        self.assertIn("FAIL P1b staging-readyz: readyz 200 but build_revision is not the QA head", out)

    def test_voice_unpublished_blocks_practice(self):
        code, out = self.run_main(self.base_args(), self.guild_routes(commands=("rank",)))
        self.assertEqual(code, 1)
        self.assertIn("FAIL P2b voice-registry", out)
        self.assertIn("BLOCKED C1 rooms-live-practice", out)

    def test_wrong_application_fails_identity(self):
        code, out = self.run_main(self.base_args(), self.guild_routes(app=OTHER_APP))
        self.assertEqual(code, 1)
        self.assertIn("FAIL P2a staging-guild-identity", out)
        self.assertIn("token is not the staging application", out)

    def test_full_pass_with_receipts(self):
        practice = self.tmp / "practice.json"
        practice.write_text(json.dumps(
            {"result": "pass", "steps": {"create": "pass", "move": "pass", "delete": "pass"}}))
        ghost = self.tmp / "ghost.json"
        ghost.write_text(json.dumps(
            {"tool": "voice-ghosts", "tracked_rooms": 3, "tracked_gone": [],
             "untracked_present": [], "clean": True}))
        code, out = self.run_main(
            self.base_args(practice_receipt=str(practice), ghost_receipt=str(ghost),
                           ci_check="pass", ci_deploy="pass"),
            self.guild_routes(commands=("rank",) + gate.WIRED_VOICE_COMMANDS))
        self.assertEqual(code, 0)
        self.assertIn(f"QA {HEAD}: PASS", out)
        receipt = json.loads(Path(self.evidence).read_text())
        self.assertEqual(receipt["result"], "pass")
        self.assertEqual(receipt["head_sha"], HEAD)
        self.assertTrue(all(c["verdict"] == "pass" for c in receipt["checks"]))

    def test_stale_contract_literal_fails(self):
        (self.root / "crates" / "bot" / "src" / "smoke_error_contract_tests.rs").write_text(
            CONTRACT_OK
            + 'assert_eq!(content, "This interaction is no longer available.");\n'
            + 'assert_eq!(content, "Manage Server permission is required.");\n')
        code, out = self.run_main(self.base_args(), self.guild_routes(
            commands=("rank",) + gate.WIRED_VOICE_COMMANDS))
        self.assertEqual(code, 1)
        self.assertIn("FAIL C3b smoke-contract-in-sync", out)

    def test_token_never_reaches_stdout_or_evidence(self):
        code, out = self.run_main(self.base_args(), self.guild_routes(
            commands=("rank",) + gate.WIRED_VOICE_COMMANDS))
        self.assertNotIn(TOKEN, out)
        self.assertNotIn(TOKEN, Path(self.evidence).read_text())


if __name__ == "__main__":
    unittest.main()
