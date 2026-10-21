"""Offline staging voice-smoke fixtures; stdlib only, no network access."""

import http.server
import io
import json
import os
import sys
import tempfile
import threading
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
# Both bodies fit BODY_CAP, so the decoder, not the size cap, must refuse them.
DEEP_ARRAY = b"[" * 32768 + b"]" * 32768
DEEP_OBJECT = b'{"":' * 13000 + b"{}" + b"}" * 13000
HEALTH_OK = json.dumps({"status": "ok"}).encode()


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
        self.requested = []
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

    def assert_no_detail_reads(self):
        app_url = f"{smoke.API}/applications/{STAGING_APP}/guilds/{STAGING_GUILD}/commands"
        self.assertEqual(self.requested, [f"{smoke.API}/users/@me", app_url])

    def test_wrong_guild_scope_refuses_at_list_and_detail(self):
        hostile = STAGING_GUILD + "?WRONG_GUILD_SENTINEL_never_echo"
        for stage in ("list", "detail"):
            for value in (OTHER_GUILD, "0", hostile, None, True):
                with self.subTest(stage=stage, value=value):
                    commands = command_list(FULL_NAMES)
                    if stage == "list":
                        commands[0]["guild_id"] = value
                    routes = self.routes(commands)
                    if stage == "detail":
                        app_url = (f"{smoke.API}/applications/{STAGING_APP}/guilds/"
                                   f"{STAGING_GUILD}/commands")
                        routes[f"{app_url}/{commands[0]['id']}"] = self.body(
                            dict(commands[0], guild_id=value))
                    code, out = self.run_main(
                        ["--guild-id", STAGING_GUILD, "--staging-url", ""], routes)
                    self.assertEqual(code, 1)
                    if stage == "list":
                        self.assertIn("FAIL rank: registered entry is not scoped"
                                      " to the staging guild", out)
                        self.assert_no_detail_reads()
                    else:
                        self.assertIn("FAIL rank: command resource is not scoped"
                                      " to the staging guild", out)
                    self.assertNotIn("WRONG_GUILD_SENTINEL", out)

    def test_omitted_guild_scope_passes_all_targets(self):
        commands = command_list(FULL_NAMES)
        for command in commands:
            del command["guild_id"]
        code, out = self.run_main(["--guild-id", STAGING_GUILD, "--staging-url", ""],
                                  self.routes(commands))
        self.assertEqual(code, 0, out)
        self.assertIn("PASS rank:", out)
        receipt = self.read_evidence()
        self.assertEqual(receipt["result"], "pass")
        self.assertEqual(receipt["guild_id"], STAGING_GUILD)

    def test_shared_command_id_across_two_targets_refuses_before_any_detail(self):
        commands = command_list(FULL_NAMES)
        commands[1]["id"] = commands[0]["id"]
        code, out = self.run_main(["--guild-id", STAGING_GUILD, "--staging-url", ""],
                                  self.routes(commands))
        self.assertEqual(code, 1)
        self.assertIn("FAIL rank: registered command id is shared with another entry",
                      out)
        self.assert_no_detail_reads()

    def test_target_id_alias_under_an_unrelated_name_refuses_before_any_detail(self):
        commands = command_list(FULL_NAMES)
        commands.append(dict(commands[0], name="unrelated-command"))
        code, out = self.run_main(["--guild-id", STAGING_GUILD, "--staging-url", ""],
                                  self.routes(commands))
        self.assertEqual(code, 1)
        self.assertIn("FAIL rank: registered command id is shared with another entry",
                      out)
        self.assert_no_detail_reads()

    def test_non_ascii_digit_ids_refuse_before_every_detail_request(self):
        # U+FF11 FULLWIDTH DIGIT ONE, U+0661 ARABIC-INDIC DIGIT ONE: int()
        # parses both, so only the ASCII-only admission regex can refuse them.
        # The single-char values pin the first class; the mixed "1" + U+0661
        # value (int() parses it as 11) pins the rest class too.
        for value in ("１", "١", "1١"):
            with self.subTest(value=value):
                commands = command_list(FULL_NAMES)
                commands[0]["id"] = value
                code, out = self.run_main(
                    ["--guild-id", STAGING_GUILD, "--staging-url", ""],
                    self.routes(commands))
                self.assertEqual(code, 1)
                self.assertIn("FAIL rank: registered entry carries no valid command id",
                              out)
                self.assert_no_detail_reads()

    def test_non_ascii_digit_versions_refuse_at_list_and_detail(self):
        for stage in ("list", "detail"):
            for value in ("１", "١", "1١"):
                with self.subTest(stage=stage, value=value):
                    commands = command_list(FULL_NAMES)
                    if stage == "list":
                        commands[0]["version"] = value
                    routes = self.routes(commands)
                    if stage == "detail":
                        app_url = (f"{smoke.API}/applications/{STAGING_APP}/guilds/"
                                   f"{STAGING_GUILD}/commands")
                        routes[f"{app_url}/{commands[0]['id']}"] = self.body(
                            dict(commands[0], version=value))
                    code, out = self.run_main(
                        ["--guild-id", STAGING_GUILD, "--staging-url", ""], routes)
                    self.assertEqual(code, 1)
                    if stage == "list":
                        self.assertIn("FAIL rank: registered entry carries"
                                      " no valid version", out)
                        self.assert_no_detail_reads()
                    else:
                        self.assertIn("FAIL rank: command resource carries"
                                      " no valid version", out)

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

    def assert_one_refusal(self, code, out, body, line):
        self.assertEqual(code, 1)
        self.assertNotIn("Traceback", out)
        self.assertNotIn(body[:8].decode(), out)
        self.assertNotIn(TOKEN, out)
        self.assertEqual([row for row in out.splitlines() if row.startswith("FAIL ")], [line])

    def test_deep_json_at_identity_refuses_without_traceback(self):
        for body in (DEEP_ARRAY, DEEP_OBJECT):
            with self.subTest(opener=body[:1]):
                code, out = self.run_main(["--guild-id", STAGING_GUILD],
                                          {f"{smoke.API}/users/@me": (200, body)})
                self.assert_one_refusal(
                    code, out, body,
                    "FAIL interactions: interactions endpoint answered 200 with invalid JSON")

    def test_deep_json_at_worker_health_refuses_without_traceback(self):
        for body in (DEEP_ARRAY, DEEP_OBJECT):
            with self.subTest(opener=body[:1]), mock.patch.object(
                    smoke, "fetch_worker", return_value=(200, body)):
                code, out = self.run_main(
                    ["--guild-id", STAGING_GUILD, "--staging-url", WORKER_URL], {})
                self.assert_one_refusal(code, out, body,
                                        "FAIL worker-health: GET /health answered without JSON")

    def test_deep_json_at_worker_readyz_refuses_without_traceback(self):
        for body in (DEEP_ARRAY, DEEP_OBJECT):
            def fake_fetch_worker(url, body=body):
                return (200, HEALTH_OK) if url.endswith("/health") else (200, body)
            with self.subTest(opener=body[:1]), mock.patch.object(
                    smoke, "fetch_worker", side_effect=fake_fetch_worker):
                code, out = self.run_main(
                    ["--guild-id", STAGING_GUILD, "--staging-url", WORKER_URL], {})
                self.assert_one_refusal(code, out, body,
                                        "FAIL worker-readyz: GET /readyz answered without JSON")


class WorkerReadyzDuplicateTests(unittest.TestCase):
    def report(self, *pairs):
        return {"components": [[name, value] for name, value in pairs]}

    def assert_rejected(self, status, pairs):
        with self.assertRaisesRegex(smoke.SmokeError, "repeats a component name"):
            smoke.parse_worker_readyz(status, self.report(*pairs))

    def test_down_then_ready_rejected(self):
        self.assert_rejected(200, [("process", "ready"), ("gateway", "down"),
                                   ("gateway", "ready")])

    def test_ready_then_down_rejected(self):
        self.assert_rejected(503, [("process", "ready"), ("gateway", "ready"),
                                   ("gateway", "down")])

    def test_same_value_repeat_rejected(self):
        self.assert_rejected(200, [("process", "ready"), ("gateway", "ready"),
                                   ("process", "ready")])

    def test_distinct_components_still_parse(self):
        state = smoke.parse_worker_readyz(
            200, self.report(("process", "ready"), ("gateway", "ready")))
        self.assertEqual(state["gateway"], "ready")

    def test_diagnostic_echoes_no_component_data(self):
        sentinel = "SENTINEL_dup_voice_abc123"
        with self.assertRaises(smoke.SmokeError) as ctx:
            smoke.parse_worker_readyz(
                503, self.report(("process", "ready"), ("gateway", "ready"),
                                 (sentinel, "ready"), (sentinel, "down")))
        self.assertIn("repeats a component name", str(ctx.exception))
        self.assertNotIn(sentinel, str(ctx.exception))

    def test_check_worker_rejects_duplicate_via_fetch_worker(self):
        dup = {"components": [["process", "ready"], ["gateway", "down"],
                              ["gateway", "ready"]]}
        health_body = json.dumps({"status": "ok"}).encode()
        readyz_body = json.dumps(dup).encode()

        def fake_fetch_worker(url):
            if url.endswith("/health"):
                return 200, health_body
            if url.endswith("/readyz"):
                return 200, readyz_body
            raise AssertionError(f"unexpected Worker GET {url}")

        with mock.patch.object(smoke, "fetch_worker", side_effect=fake_fetch_worker):
            results, info = smoke.check_worker("https://two-bot-next-staging.5150.workers.dev")
        self.assertIsNone(info)
        self.assertEqual(len(results), 2)
        self.assertEqual(results[0].verdict, "pass")
        self.assertEqual(results[1].verdict, "fail")
        self.assertIn("repeats a component name", results[1].reason)
        self.assertNotIn("SENTINEL", results[1].reason)

    def test_check_worker_accepts_distinct_via_fetch_worker(self):
        ok = {"components": [["process", "ready"], ["gateway", "ready"]],
              "build_revision": "abc123"}
        health_body = json.dumps({"status": "ok"}).encode()
        readyz_body = json.dumps(ok).encode()

        def fake_fetch_worker(url):
            if url.endswith("/health"):
                return 200, health_body
            if url.endswith("/readyz"):
                return 200, readyz_body
            raise AssertionError(f"unexpected Worker GET {url}")

        with mock.patch.object(smoke, "fetch_worker", side_effect=fake_fetch_worker):
            results, info = smoke.check_worker("https://two-bot-next-staging.5150.workers.dev")
        self.assertIsNotNone(info)
        self.assertTrue(all(r.verdict == "pass" for r in results))

    def test_unknown_ready_down_starting_refused(self):
        for extra_status, http_status in (("ready", 200), ("down", 503), ("starting", 503)):
            with self.subTest(extra_status=extra_status):
                with self.assertRaisesRegex(smoke.SmokeError, "unknown component name"):
                    smoke.parse_worker_readyz(
                        http_status, self.report(("process", "ready"), ("gateway", "ready"),
                                                ("voice", extra_status)))

    def test_unknown_sentinel_echoes_no_component_data(self):
        sentinel = "SENTINEL_unknown_voice_abc123"
        with self.assertRaises(smoke.SmokeError) as ctx:
            smoke.parse_worker_readyz(
                200, self.report(("process", "ready"), ("gateway", "ready"),
                                 (sentinel, "ready")))
        self.assertIn("unknown component name", str(ctx.exception))
        self.assertNotIn(sentinel, str(ctx.exception))

    def test_legitimate_optional_components_still_parse(self):
        for pairs, status in (
            ([("process", "ready"), ("gateway", "ready")], 200),
            ([("process", "ready"), ("gateway", "ready"),
              ("database", "ready")], 200),
            ([("process", "ready"), ("gateway", "ready"),
              ("database", "ready"), ("token_invalid", "ready")], 200),
            ([("process", "ready"), ("gateway", "starting"),
              ("database", "ready"), ("token_invalid", "ready")], 503),
        ):
            with self.subTest(pairs=pairs):
                state = smoke.parse_worker_readyz(status, self.report(*pairs))
                self.assertEqual(set(state), {name for name, _ in pairs})

    def test_missing_required_component_refused(self):
        with self.assertRaisesRegex(smoke.SmokeError, "lacks gateway"):
            smoke.parse_worker_readyz(200, self.report(("process", "ready"),))

    def test_check_worker_rejects_unknown_via_fetch_worker(self):
        unknown = {"components": [["process", "ready"], ["gateway", "ready"],
                                   ["voice", "ready"]]}
        health_body = json.dumps({"status": "ok"}).encode()
        readyz_body = json.dumps(unknown).encode()

        def fake_fetch_worker(url):
            if url.endswith("/health"):
                return 200, health_body
            if url.endswith("/readyz"):
                return 200, readyz_body
            raise AssertionError(f"unexpected Worker GET {url}")

        with mock.patch.object(smoke, "fetch_worker", side_effect=fake_fetch_worker):
            results, info = smoke.check_worker("https://two-bot-next-staging.5150.workers.dev")
        self.assertIsNone(info)
        self.assertEqual(results[1].verdict, "fail")
        self.assertIn("unknown component name", results[1].reason)
        self.assertNotIn("voice", results[1].reason)


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


if __name__ == "__main__":
    unittest.main()
