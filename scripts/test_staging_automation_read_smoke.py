"""Offline automation-read-smoke fixtures; stdlib only, no network access."""

import http.server
import io
import json
import os
import sys
import tempfile
import threading
import unittest
from contextlib import redirect_stdout
from importlib import import_module
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
smoke = import_module("staging_automation_read_smoke")
preview = import_module("staging_preview_readback")

STAGING_GUILD = "1545644954272137297"
LIVE_GUILD = "326474832151838730"
OTHER_GUILD = "9999999999999999999"
STAGING_APP = "1469137636663758888"
OTHER_APP = "1111111111111111111"
TOKEN = "RAW_SECRET_SENTINEL_never_disclose_bot_token"
# Both bodies fit BODY_CAP, so the decoder, not the size cap, must refuse them.
DEEP_ARRAY = b"[" * 32768 + b"]" * 32768
DEEP_OBJECT = b'{"":' * 13000 + b"{}" + b"}" * 13000

# Discord command objects always carry the owning application_id.
COMMANDS = [
    {"id": "1549621908675366953", "name": "rank", "type": 1, "application_id": STAGING_APP,
     "guild_id": STAGING_GUILD, "version": "1000000000000000001"},
    {"id": "2000000000000000001", "name": "command-list", "type": 1,
     "application_id": STAGING_APP, "guild_id": STAGING_GUILD, "version": "1000000000000000002"},
    {"id": "2000000000000000002", "name": "schedule-list", "type": 1,
     "application_id": STAGING_APP, "guild_id": STAGING_GUILD, "version": "1000000000000000003"},
    {"id": "2000000000000000003", "name": "feed-list", "type": 1,
     "application_id": STAGING_APP, "guild_id": STAGING_GUILD, "version": "1000000000000000004"},
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

    def test_deep_json_at_identity_refuses_without_traceback(self):
        for body in (DEEP_ARRAY, DEEP_OBJECT):
            with self.subTest(opener=body[:1]):
                code, out = self.run_main(STAGING_GUILD, {f"{smoke.API}/users/@me": (200, body)})
                self.assertEqual(code, 1)
                self.assertNotIn("Traceback", out)
                self.assertNotIn(body[:8].decode(), out)
                self.assertNotIn(TOKEN, out)
                self.assertEqual([row for row in out.splitlines() if row.startswith("FAIL ")],
                                 ["FAIL interactions: interactions endpoint answered 200 with invalid JSON"])


class BodyCapRefusalTests(unittest.TestCase):
    """Oversized bodies are refused before JSON parsing (M4.42)."""

    def setUp(self):
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

    def padded(self, value, size):
        raw = json.dumps(value).encode()
        self.assertLessEqual(len(raw), size, "fixture must fit the target size")
        return raw + b" " * (size - len(raw))

    def run_main(self, routes):
        with mock.patch.dict(os.environ, {"DISCORD_STAGING_BOT_TOKEN": TOKEN}, clear=True):
            out = io.StringIO()
            with redirect_stdout(out):
                code = smoke.main(["--guild-id", STAGING_GUILD, "--evidence", self.evidence],
                                  fetch_fn=self.fetch(routes))
        return code, out.getvalue()

    def base_routes(self):
        app_url = f"{smoke.API}/applications/{STAGING_APP}/guilds/{STAGING_GUILD}/commands"
        mapping = {f"{smoke.API}/users/@me": (200, json.dumps(
            {"id": STAGING_APP}).encode()),
            app_url: (200, json.dumps(COMMANDS).encode())}
        for command in COMMANDS:
            mapping[f"{app_url}/{command['id']}"] = (200, json.dumps(command).encode())
        return mapping, app_url

    def test_exact_cap_valid_input_passes(self):
        routes, app_url = self.base_routes()
        identity = json.dumps({"id": STAGING_APP}).encode()
        routes[f"{smoke.API}/users/@me"] = (200, self.padded({"id": STAGING_APP}, smoke.BODY_CAP))
        self.assertEqual(len(routes[f"{smoke.API}/users/@me"][1]), smoke.BODY_CAP)
        code, out = self.run_main(routes)
        self.assertEqual(code, 0, out)
        self.assertIn("PASS rank:", out)

    def test_valid_json_padded_past_cap_refuses_before_parsing(self):
        routes, _ = self.base_routes()
        sentinel = "SENTINEL_BODY_automation_padded"
        base = json.dumps({"id": STAGING_APP, "note": sentinel}).encode()
        body = base + b" " * (smoke.BODY_CAP + 1 - len(base))
        self.assertEqual(len(body), smoke.BODY_CAP + 1)
        # Still valid JSON (trailing whitespace): the old parser would accept.
        self.assertEqual(json.loads(body)["note"], sentinel)
        routes[f"{smoke.API}/users/@me"] = (200, body)
        code, out = self.run_main(routes)
        self.assertEqual(code, 1, out)
        self.assertIn("interactions endpoint answered over the body cap", out)
        self.assertNotIn(sentinel, out)
        self.assertNotIn("SENTINEL_BODY", out)

    def test_cap_length_valid_prefix_with_hidden_tail_refuses(self):
        routes, _ = self.base_routes()
        prefix = self.padded({"id": STAGING_APP}, smoke.BODY_CAP)
        self.assertEqual(json.loads(prefix)["id"], STAGING_APP)
        # Hidden tail beyond the cap: the first CAP bytes alone are valid JSON,
        # so an exact-cap read would silently accept the prefix.
        routes[f"{smoke.API}/users/@me"] = (200, prefix + b"X")
        code, out = self.run_main(routes)
        self.assertEqual(code, 1, out)
        self.assertIn("interactions endpoint answered over the body cap", out)
        self.assertNotIn("X", out.split("over the body cap")[0][-80:])

    def test_transport_reads_bounded_cap_plus_one_both_paths(self):
        import urllib.error
        seen = []

        class Response:
            status = 200

            def __init__(self, payload):
                self.payload = payload

            def read(self, n):
                seen.append(n)
                return self.payload

            def close(self):
                pass

            def __enter__(self):
                return self

            def __exit__(self, *exc):
                return False

        class Opener:
            def __init__(self, behaviour):
                self.behaviour = behaviour

            def open(self, request, timeout=None):
                if self.behaviour == "ok":
                    return Response(b"{}")
                raise urllib.error.HTTPError(request.full_url, 500, "Server Error", {}, Response(b"x"))

        fetch = smoke.make_fetch(TOKEN)
        with mock.patch.object(smoke.urllib.request, "build_opener",
                               return_value=Opener("ok")):
            fetch("https://discord.com/api/v10/users/@me")
        self.assertEqual(seen, [smoke.BODY_CAP + 1])
        seen.clear()
        with mock.patch.object(smoke.urllib.request, "build_opener",
                               return_value=Opener("error")):
            fetch("https://discord.com/api/v10/users/@me")
        # HTTPError path also reads a bounded amount, never unbounded.
        self.assertEqual(seen, [smoke.BODY_CAP + 1])


class _Loopback:
    """Isolated 127.0.0.1 server that records every request it receives."""

    def __init__(self, handler_for):
        self.hits = []
        outer = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                outer.hits.append((self.path, self.headers.get("Authorization")))
                status, headers, body = handler_for(self.path)
                self.send_response(status)
                for key, value in headers.items():
                    self.send_header(key, value)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *args):
                pass

        self.server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
        self.base = f"http://127.0.0.1:{self.server.server_port}"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()


class RedirectTransportTests(unittest.TestCase):
    """Real opener and handlers against loopback: no redirect is ever followed."""

    def setUp(self):
        env = mock.patch.dict(os.environ, {"NO_PROXY": "*", "no_proxy": "*"})
        env.start()
        self.addCleanup(env.stop)
        for name in ("HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"):
            os.environ.pop(name, None)

    def serve(self, handler_for):
        server = _Loopback(handler_for)
        self.addCleanup(server.close)
        return server

    def test_every_redirect_status_is_refused_with_zero_follow_ups(self):
        for status in (301, 302, 303, 307, 308):
            for cross_origin in (False, True):
                with self.subTest(status=status, cross_origin=cross_origin):
                    destination = self.serve(lambda path: (200, {}, b"LEAKED_DESTINATION_BODY"))
                    target = (destination.base if cross_origin else "") + "/second"
                    origin = None

                    def route(path, status=status, target=target):
                        if path == "/first":
                            return status, {"Location": origin.base + target
                                            if not target.startswith("http") else target}, \
                                b"REDIRECT_BODY_SENTINEL"
                        return 200, {}, b"{}"

                    origin = self.serve(route)
                    with self.assertRaises(smoke.SmokeError) as raised:
                        smoke.make_fetch(TOKEN)(origin.base + "/first")
                    self.assertEqual([path for path, _ in origin.hits], ["/first"])
                    self.assertEqual(destination.hits, [])
                    text = str(raised.exception)
                    self.assertEqual(
                        text, "refusing: authenticated read answered a redirect (not followed)")
                    for secret in (TOKEN, "REDIRECT_BODY_SENTINEL", "/second",
                                   "127.0.0.1", "LEAKED_DESTINATION_BODY"):
                        self.assertNotIn(secret, text)

    def test_healthy_and_auth_denied_controls_keep_their_behavior(self):
        server = self.serve(lambda path: (401, {}, b"denied") if path == "/denied"
                            else (200, {}, b'{"ok": true}'))
        fetch = smoke.make_fetch(TOKEN)
        self.assertEqual(fetch(server.base + "/healthy"), (200, b'{"ok": true}'))
        self.assertEqual(fetch(server.base + "/denied"), (401, b"denied"))
        self.assertEqual([auth for _, auth in server.hits], ["Bot " + TOKEN] * 2)

    def test_redirect_through_get_json_is_a_smoke_error_with_no_secret(self):
        server = self.serve(lambda path: (302, {"Location": "/elsewhere"}, b"BODY_SENTINEL"))
        with self.assertRaises(smoke.SmokeError) as raised:
            smoke.get_json(smoke.make_fetch(TOKEN), server.base + "/x")
        self.assertEqual(len(server.hits), 1)
        self.assertNotIn(TOKEN, str(raised.exception))
        self.assertNotIn("BODY_SENTINEL", str(raised.exception))


class PreviewReadbackTests(unittest.TestCase):
    def test_unexpected_transport_error_is_redacted(self):
        stdout = io.StringIO()
        with mock.patch.dict(os.environ, {"STAGING_USE_EXPIRES_AT": "90000"}, clear=True), \
                mock.patch.object(preview.time, "time", return_value=1), redirect_stdout(stdout), \
                mock.patch.object(preview, "observe", side_effect=ValueError(TOKEN)), \
                self.assertRaisesRegex(SystemExit, "invalid transport or response") as error:
            preview.main()
        self.assertNotIn(TOKEN, str(error.exception))
        self.assertEqual(stdout.getvalue(), "")

    def test_in_flight_window_expiry_emits_no_receipt(self):
        stdout = io.StringIO()
        with mock.patch.dict(os.environ, {"STAGING_USE_EXPIRES_AT": "90000"}, clear=True), \
                mock.patch.object(preview.time, "time", return_value=1), redirect_stdout(stdout), \
                mock.patch.object(preview, "observe", side_effect=lambda *_:
                                  preview.signal.raise_signal(preview.signal.SIGALRM)), \
                self.assertRaisesRegex(SystemExit, "staging window expired"):
            preview.main()
        self.assertEqual(stdout.getvalue(), "")

    def test_exact_staging_preview_receipt(self):
        for fault, expected_reads in (("none", 3), ("live-guild", 0), ("missing-token", 0),
                                      ("expired", 0), ("wrong-sha", 0), ("wrong-attempt", 0),
                                      ("wrong-app", 1), ("absent-command", 2), ("conflicting-version", 3),
                                      ("permission", 3), ("malformed-option", 3), ("wrong-bounds", 3),
                                      ("autocomplete", 3), ("drift", 3)):
            with self.subTest(fault=fault):
                self.check_case(fault, expected_reads)

    def check_case(self, fault, expected_reads):
        case = SimpleNamespace(expected_sha="a" * 40, build_id="123-1", expires_at=90_000,
                               guild_id=STAGING_GUILD,
                               staging_url="https://two-bot-next-staging.example.workers.dev")
        command = {**COMMANDS[0], "name": "automod-preview", "default_member_permissions": "32",
                   "description": TOKEN, "options": [
                       {"name": "text", "type": 3, "required": True, "min_length": 1, "max_length": 2000},
                       {"name": "mentions", "type": 4, "min_value": 0, "max_value": 100},
                       {"name": "attachments", "type": 3, "required": False, "max_length": 1000},
                   ]}
        endpoint = f"{smoke.API}/applications/{STAGING_APP}/guilds/{STAGING_GUILD}/commands"
        detail = dict(command)
        reads = []
        health_reads = []
        if fault == "live-guild":
            case.guild_id = LIVE_GUILD
        if fault == "permission":
            detail["default_member_permissions"] = None
        if fault == "conflicting-version":
            detail["version"] = "1000000000000000099"
        option_faults = {"malformed-option": {"required": 1}, "wrong-bounds": {"max_length": 2001},
                         "autocomplete": {"autocomplete": True}}
        if fault in option_faults:
            detail["options"] = [{**command["options"][0], **option_faults[fault]}, *command["options"][1:]]

        def health_fetch(url):
            health_reads.append(url)
            if url.endswith("/health"):
                return 200, {}, b'{"status":"ok"}'
            wrong = fault == "wrong-attempt" or (fault == "drift" and len(health_reads) == 4)
            return 200, {}, json.dumps({
                "components": [[n, "ready"] for n in preview.health.REQUIRED_COMPONENTS],
                "jobs": {}, "build_revision": "b" * 40 if fault == "wrong-sha" else case.expected_sha,
                "build_id": "123-2" if wrong else case.build_id,
            }).encode()

        def discord_fetch(url):
            reads.append(url)
            values = {smoke.API + "/users/@me": {"id": OTHER_APP if fault == "wrong-app" else STAGING_APP},
                      endpoint: [] if fault == "absent-command" else [command],
                      f"{endpoint}/{command['id']}": detail}
            return 200, json.dumps(values[url]).encode()

        clock = lambda: 89 if fault == "expired" else 1
        token = "" if fault == "missing-token" else TOKEN
        if fault == "none":
            receipt = preview.observe(case, token, clock=clock, health_fetch=health_fetch,
                                      make_fetch=lambda _: discord_fetch)
            self.assertEqual(receipt, {
                "build_revision": case.expected_sha, "build_id": case.build_id, "command": "automod-preview",
                "command_id": command["id"], "version": command["version"], "registry": "pass",
                "transport": "GET-only", "invocation": False,
            })
            self.assertEqual(health_reads, [case.staging_url + path for path in ("/health", "/readyz") * 2])
            self.assertEqual(reads, [smoke.API + "/users/@me", endpoint, f"{endpoint}/{command['id']}"])
            self.assertNotIn(TOKEN, json.dumps(receipt))
        else:
            with self.assertRaises((ValueError, smoke.SmokeError)) as error:
                preview.observe(case, token, clock=clock, health_fetch=health_fetch,
                                make_fetch=lambda _: discord_fetch)
            self.assertNotIn(TOKEN, str(error.exception))
            if fault in {"live-guild", "missing-token", "expired"}:
                self.assertEqual(health_reads, [])
        self.assertEqual(len(reads), expected_reads)


if __name__ == "__main__":
    unittest.main()
