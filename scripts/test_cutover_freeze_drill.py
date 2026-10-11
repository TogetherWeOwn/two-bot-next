"""Offline cutover freeze-drill harness checks; stdlib only, no network access."""

import http.server
import io
import json
import os
import sys
import tempfile
import threading
import unittest
from contextlib import redirect_stdout
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
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
        # Clears the lockdown bits from allow, sets them in deny, preserves the rest.
        locked = str(drill.LOCKDOWN_BITS)
        self.assertEqual(drill.LOCKDOWN_BITS, 2048 | 64 | (1 << 35) | (1 << 36) | (1 << 38))
        allow, deny = drill.plan_lockdown_masks("4096", "0")
        self.assertEqual((allow, deny), ("4096", locked))
        allow, deny = drill.plan_lockdown_masks("6144", "2048")
        self.assertEqual((allow, deny), ("4096", locked))
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


CHANNEL = "1000000000000000002"
CHANNEL_PATH = f"/api/v10/channels/{CHANNEL}"
MESSAGES_PATH = f"{CHANNEL_PATH}/messages"
OVERWRITE_PATH = f"{CHANNEL_PATH}/permissions/{STAGING_GUILD}"
NOTICE_ID = "9000000000000000001"
NOTICE_PATH = f"{MESSAGES_PATH}/{NOTICE_ID}"
COMMANDS_PATH = (f"/api/v10/applications/{drill.STAGING_APPLICATION_ID}"
                 f"/guilds/{STAGING_GUILD}/commands")
CHANNEL_JSON = {"id": CHANNEL, "guild_id": STAGING_GUILD, "name": "cutover-drill",
                "rate_limit_per_user": 0, "permission_overwrites": []}
REMOTE_SENTINEL = "REMOTE_BODY_SENTINEL"


def discord_routes(overrides):
    routes = {
        ("GET", CHANNEL_PATH): (200, {}, json.dumps(CHANNEL_JSON).encode()),
        ("POST", MESSAGES_PATH): (200, {}, json.dumps({"id": NOTICE_ID}).encode()),
        ("PATCH", CHANNEL_PATH): (200, {}, json.dumps(CHANNEL_JSON).encode()),
        ("PUT", OVERWRITE_PATH): (204, {}, b""),
        ("DELETE", OVERWRITE_PATH): (204, {}, b""),
        ("DELETE", NOTICE_PATH): (204, {}, b""),
        ("GET", COMMANDS_PATH): (200, {}, json.dumps([{"id": "1", "name": "ping"}]).encode()),
    }
    routes.update(overrides)
    return routes


class LoopbackDiscord:
    """127.0.0.1 stand-in for Discord: canned replies per (method, path), every hit recorded."""

    def __init__(self):
        self.routes = {}
        self.hits = []
        fixture = self

        class Handler(BaseHTTPRequestHandler):
            def serve(self):
                length = int(self.headers.get("Content-Length") or 0)
                if length:
                    self.rfile.read(length)
                fixture.hits.append((self.command, self.path, self.headers.get("Authorization")))
                status, headers, body = fixture.routes.get(
                    (self.command, self.path), (404, {}, b""))
                self.send_response(status)
                for name, value in headers.items():
                    self.send_header(name, value)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            do_GET = do_POST = do_PUT = do_PATCH = do_DELETE = serve

            def log_message(self, *args):
                pass

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.base = f"http://127.0.0.1:{self.server.server_address[1]}"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()


class LiveTransportTests(unittest.TestCase):
    def setUp(self):
        self.origin = LoopbackDiscord()
        self.elsewhere = LoopbackDiscord()
        self.addCleanup(self.origin.close)
        self.addCleanup(self.elsewhere.close)
        self.origin.routes = discord_routes({})
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.evidence = str(Path(tmp.name) / "evidence.json")
        for patcher in (mock.patch.dict(os.environ, {"no_proxy": "127.0.0.1"}),
                        mock.patch.object(drill, "API", self.origin.base + "/api/v10")):
            patcher.start()
            self.addCleanup(patcher.stop)

    def test_every_redirect_status_stops_at_the_first_reply(self):
        call = drill.live_transport(TOKEN)
        ops = (("get_channel", {"channel_id": CHANNEL}, "GET", CHANNEL_PATH),
               ("post_notice", {"channel_id": CHANNEL, "content": "x"}, "POST", MESSAGES_PATH),
               ("patch_overwrite", {"channel_id": CHANNEL, "guild_id": STAGING_GUILD,
                                    "allow": "0", "deny": "0"}, "PUT", OVERWRITE_PATH))
        for op, kwargs, method, path in ops:
            for status in drill.REDIRECT_STATUSES:
                for same_origin in (False, True):
                    with self.subTest(op=op, status=status, same_origin=same_origin):
                        location = (f"{self.origin.base}/api/v10/elsewhere" if same_origin
                                    else f"{self.elsewhere.base}/stolen")
                        self.origin.routes = {(method, path): (
                            status, {"Location": location}, REMOTE_SENTINEL.encode())}
                        self.origin.hits.clear()
                        self.elsewhere.hits.clear()
                        with self.assertRaises(drill.DrillError) as caught:
                            call(op, **kwargs)
                        self.assertEqual(
                            str(caught.exception),
                            "refusing: authenticated Discord request answered a redirect (not followed)")
                        self.assertEqual([(m, p) for m, p, _ in self.origin.hits],
                                         [(method, path)])
                        self.assertEqual(self.origin.hits[0][2], f"Bot {TOKEN}")
                        self.assertEqual(self.elsewhere.hits, [])

    def test_malformed_2xx_replies_become_drill_errors_and_restore_runs(self):
        cases = (
            ("notice-not-json", {("POST", MESSAGES_PATH): (200, {}, b"<html>REMOTE_BODY_SENTINEL")},
             "discord answered a non-JSON body on POST"),
            ("notice-not-object", {("POST", MESSAGES_PATH): (200, {}, b"[]")},
             "discord answered an unexpected body shape on POST"),
            ("notice-without-id", {("POST", MESSAGES_PATH): (200, {}, b"{}")},
             "discord notice reply carried no message id"),
            ("commands-not-json", {("GET", COMMANDS_PATH): (200, {}, b"\x80REMOTE_BODY_SENTINEL")},
             "discord answered a non-JSON body on GET"),
            ("commands-not-list", {("GET", COMMANDS_PATH): (200, {}, b'{"commands": []}')},
             "discord answered an unexpected body shape on GET"),
            ("slowmode-not-json", {("PATCH", CHANNEL_PATH): (200, {}, b"REMOTE_BODY_SENTINEL")},
             "discord answered a non-JSON body on PATCH"),
            ("slowmode-too-deep", {("PATCH", CHANNEL_PATH): (200, {}, b"[" * 65536)},
             "discord answered a non-JSON body on PATCH"),
        )
        call = drill.live_transport(TOKEN)
        for name, overrides, reason in cases:
            with self.subTest(case=name):
                self.origin.routes = discord_routes(overrides)
                self.origin.hits.clear()
                outcome = drill.run_drill(STAGING_GUILD, CHANNEL, REASON, call)
                self.assertEqual(outcome["result"], "fail")
                failed = [s for s in outcome["steps"] if s["result"] == "fail"]
                self.assertEqual(failed[0]["detail"], reason)
                names = [s["name"] for s in outcome["steps"]]
                for restore in ("restore-slowmode", "restore-unlock", "restore-notice-delete"):
                    self.assertIn(restore, names)
                blob = json.dumps(outcome)
                self.assertNotIn(TOKEN, blob)
                self.assertNotIn(REMOTE_SENTINEL, blob)

    def test_surface_failure_sends_every_restore_request(self):
        self.origin.routes = discord_routes(
            {("GET", COMMANDS_PATH): (200, {}, b'{"commands": []}')})
        outcome = drill.run_drill(STAGING_GUILD, CHANNEL, REASON, drill.live_transport(TOKEN))
        self.assertEqual(outcome["result"], "fail")
        self.assertEqual(outcome["restore"]["restored"], False)
        requested = {(m, p) for m, p, _ in self.origin.hits}
        self.assertLessEqual({("PATCH", CHANNEL_PATH), ("DELETE", OVERWRITE_PATH),
                              ("DELETE", NOTICE_PATH)}, requested)
        restore_rows = {s["name"]: s["result"] for s in outcome["steps"]
                        if s["name"].startswith("restore-")}
        self.assertEqual(restore_rows, {"restore-slowmode": "pass", "restore-unlock": "pass",
                                        "restore-notice-delete": "pass"})

    def test_cli_live_run_with_malformed_reply_still_writes_evidence(self):
        self.origin.routes = discord_routes(
            {("GET", COMMANDS_PATH): (200, {}, b"\x80REMOTE_BODY_SENTINEL")})
        with mock.patch.dict(os.environ, {"DISCORD_STAGING_BOT_TOKEN": TOKEN}, clear=True):
            out = io.StringIO()
            with redirect_stdout(out):
                code = drill.main(["--live", "--confirm-staging", "--guild-id", STAGING_GUILD,
                                   "--channel-id", CHANNEL, "--reason", REASON,
                                   "--evidence", self.evidence])
        self.assertEqual(code, 1)
        receipt = json.loads(Path(self.evidence).read_text())
        self.assertEqual(receipt["result"], "fail")
        self.assertEqual(receipt["transport"], drill.LIVE_TRANSPORT)
        self.assertIn("restore-notice-delete", [s["name"] for s in receipt["steps"]])
        blob = out.getvalue() + json.dumps(receipt)
        self.assertNotIn(TOKEN, blob)
        self.assertNotIn(REMOTE_SENTINEL, blob)


class _Loopback:
    """Isolated server that records the method, path and Authorization header."""

    def __init__(self, handler_for):
        self.hits = []
        outer = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def respond(self):
                outer.hits.append((self.command, self.path, self.headers.get("Authorization")))
                status, headers, body = handler_for(self.command, self.path)
                self.send_response(status)
                for key, value in headers.items():
                    self.send_header(key, value)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            do_GET = do_POST = do_PATCH = do_PUT = do_DELETE = respond

            def log_message(self, *args):
                pass

        self.server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
        self.base = f"http://127.0.0.1:{self.server.server_port}"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()


class RedirectTransportTests(unittest.TestCase):
    """Real urllib opener and loopback transport; no Discord or Worker calls."""

    def setUp(self):
        env = mock.patch.dict(os.environ, {"NO_PROXY": "*", "no_proxy": "*",
                                       "HTTP_PROXY": "", "http_proxy": "",
                                       "ALL_PROXY": "", "all_proxy": ""})
        env.start()
        self.addCleanup(env.stop)

    def serve(self, handler_for):
        server = _Loopback(handler_for)
        self.addCleanup(server.close)
        return server

    def test_get_redirects_refuse_same_and_cross_origin_without_followup(self):
        destination = self.serve(lambda method, path: (200, {}, b"DESTINATION_BODY"))
        for status in (301, 302, 303, 307, 308):
            for cross_origin in (False, True):
                with self.subTest(status=status, cross_origin=cross_origin):
                    target = destination.base + "/second" if cross_origin else "/second"
                    origin = self.serve(lambda method, path, status=status, target=target:
                                        (status, {"Location": target}, b"REDIRECT_BODY"))
                    with mock.patch.object(drill, "API", origin.base):
                        with self.assertRaises(drill.DrillError) as raised:
                            drill.live_transport(TOKEN)("get_channel", channel_id="first")
                    self.assertEqual(origin.hits, [("GET", "/channels/first", "Bot " + TOKEN)])
                    self.assertEqual(destination.hits, [])
                    self.assertEqual(str(raised.exception),
                                     "refusing: authenticated Discord request answered a redirect (not followed)")
                    for secret in (TOKEN, "REDIRECT_BODY", "/second", "127.0.0.1"):
                        self.assertNotIn(secret, str(raised.exception))

    def test_post_redirects_refuse_without_replaying_token_or_body(self):
        destination = self.serve(lambda method, path: (200, {}, b"DESTINATION_BODY"))
        for status in (301, 302, 303, 307, 308):
            for cross_origin in (False, True):
                with self.subTest(status=status, cross_origin=cross_origin):
                    destination.hits.clear()
                    target = destination.base + "/second" if cross_origin else "/second"
                    origin = self.serve(lambda method, path, status=status, target=target:
                                        (status, {"Location": target}, b"REDIRECT_BODY"))
                    with mock.patch.object(drill, "API", origin.base):
                        with self.assertRaises(drill.DrillError) as raised:
                            drill.live_transport(TOKEN)("post_notice", channel_id="first",
                                                        content="fixture")
                    self.assertEqual(origin.hits,
                                     [("POST", "/channels/first/messages", "Bot " + TOKEN)])
                    self.assertEqual(destination.hits, [])
                    self.assertEqual(str(raised.exception),
                                     "refusing: authenticated Discord request answered a redirect (not followed)")

    def test_other_writes_refuse_redirects_without_followup(self):
        destination = self.serve(lambda method, path: (200, {}, b"DESTINATION_BODY"))
        cases = (("PATCH", "patch_slowmode", {"channel_id": "first", "seconds": 30}),
                 ("PUT", "patch_overwrite", {"channel_id": "first", "guild_id": STAGING_GUILD,
                                              "allow": "0", "deny": "2048"}),
                 ("DELETE", "delete_notice", {"channel_id": "first", "notice_id": "fixture"}))
        for method, op, kwargs in cases:
            with self.subTest(method=method):
                origin = self.serve(lambda request_method, path:
                                    (302, {"Location": destination.base + "/second"},
                                     b"REDIRECT_BODY"))
                with mock.patch.object(drill, "API", origin.base):
                    with self.assertRaises(drill.DrillError) as raised:
                        drill.live_transport(TOKEN)(op, **kwargs)
                self.assertEqual(len(origin.hits), 1)
                self.assertEqual(origin.hits[0][0], method)
                self.assertEqual(origin.hits[0][2], "Bot " + TOKEN)
                self.assertEqual(destination.hits, [])
                self.assertEqual(str(raised.exception),
                                 "refusing: authenticated Discord request answered a redirect (not followed)")

    def test_healthy_and_auth_denied_controls(self):
        origin = self.serve(lambda method, path: (401, {}, b"denied") if path.endswith("denied")
                            else (200, {}, b'{"ok": true}'))
        with mock.patch.object(drill, "API", origin.base):
            call = drill.live_transport(TOKEN)
            self.assertEqual(call("get_channel", channel_id="healthy"), {"ok": True})
            with self.assertRaises(drill.DrillError) as raised:
                call("get_channel", channel_id="denied")
        self.assertEqual(str(raised.exception), "discord answered 401 on GET")
        self.assertEqual([auth for _, _, auth in origin.hits], ["Bot " + TOKEN] * 2)


if __name__ == "__main__":
    unittest.main()
