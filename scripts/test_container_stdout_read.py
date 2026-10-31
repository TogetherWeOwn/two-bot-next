"""Offline fixtures for scripts/container_stdout_read.py; loopback HTTP only, no Cloudflare."""

import copy
from datetime import datetime, timezone
import http.server
import io
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import socket
import threading
import time
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import container_stdout_read as reader  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent
SCRIPT = Path(__file__).with_name("container_stdout_read.py")
FIXTURES = Path(__file__).with_name("fixtures")
REQUEST = json.loads((FIXTURES / "container_stdout_read_request.json").read_text())
RESPONSE = json.loads((FIXTURES / "container_stdout_read_response.json").read_text())
SERVICE = "11111111-2222-4333-8444-555555555555"
ACCOUNT = "0123456789abcdef0123456789abcdef"
END = "2026-10-11T12:20:00Z"
NOW = datetime(2026, 10, 11, 12, 30, 0, tzinfo=timezone.utc)
QUERY_ID = REQUEST["queryId"]
TOKEN = "tok-" + "A1b2C3d4" * 4  # fake; assembled so no scanner mistakes it for a credential

# Secret-like values are assembled at run time: the fixtures stay scanner-clean.
HOSTILE = {
    "url": "post" + "gresql://svc:hunter2@db.example.invalid/two_bot?sslmode=require",
    "bearer": "Bearer " + "q" * 24,
    "keyed": "api_key=" + "plain-secret-value",
    "discord": ".".join(["M" + "A" * 23, "abcdef", "z" * 27]),
    "opaque": "Zm9v" + "9" * 40,
    "snowflake": "9" * 18,
}


def fixture_response(**changes):
    packet = copy.deepcopy(RESPONSE)
    packet.update(changes)
    return packet


def hostile_event(message, **source):
    return {"dataset": "containers", "timestamp": 1791721190000,
            "source": {"message": message, **source},
            "$metadata": {"service": SERVICE, "level": "warn", "message": message}}


_FIXTURE = object()


class FakeTransport:
    """Records every call; answers the accounts list and the telemetry query."""

    def __init__(self, query_answer=_FIXTURE, accounts=None):
        self.calls = []
        self.query_answer = RESPONSE if query_answer is _FIXTURE else query_answer
        self.accounts = [{"id": ACCOUNT}] if accounts is None else accounts

    def __call__(self, method, path, body=None):
        self.calls.append((method, path, body))
        if path == "/accounts":
            return {"success": True, "result": self.accounts}
        return self.query_answer


def run_main(argv, transport=None, env=None):
    out, err = io.StringIO(), io.StringIO()
    code = reader.main(argv, env={} if env is None else env, transport=transport,
                       now=lambda: NOW, stdout=out, stderr=err)
    return code, out.getvalue(), err.getvalue()


def read_with(transport, **overrides):
    args = {"transport": transport, "service": SERVICE, "minutes": 5,
            "end": datetime(2026, 10, 11, 12, 20, tzinfo=timezone.utc), "limit": 100,
            "contains": None, "account_id": ACCOUNT, "query_id": QUERY_ID}
    args.update(overrides)
    return reader.read(args.pop("transport"), args.pop("service"), args.pop("minutes"),
                       args.pop("end"), args.pop("limit"), args.pop("contains"),
                       args.pop("account_id"), args.pop("query_id"))


class RequestShapeTests(unittest.TestCase):
    def test_query_body_matches_the_pinned_fixture(self):
        transport = FakeTransport()
        read_with(transport)
        self.assertEqual(transport.calls, [
            ("POST", f"/accounts/{ACCOUNT}/workers/observability/telemetry/query", REQUEST)])

    def test_contains_adds_one_message_filter_and_nothing_else(self):
        transport = FakeTransport()
        read_with(transport, contains="durable gateway failed")
        body = transport.calls[0][2]
        expected = copy.deepcopy(REQUEST)
        expected["parameters"]["filters"].append(
            {"key": "$metadata.message", "type": "string", "operation": "includes",
             "value": "durable gateway failed"})
        self.assertEqual(body, expected)

    def test_a_supplied_account_skips_the_accounts_list(self):
        transport = FakeTransport()
        code, _, _ = run_main(["--service", SERVICE, "--end", END, "--account-id", ACCOUNT], transport)
        self.assertEqual(code, 0)
        self.assertEqual([call[0] for call in transport.calls], ["POST"])

    def test_the_only_visible_account_is_used_otherwise(self):
        transport = FakeTransport()
        code, _, _ = run_main(["--service", SERVICE, "--end", END], transport)
        self.assertEqual(code, 0)
        self.assertEqual([call[:2] for call in transport.calls],
                         [("GET", "/accounts"),
                          ("POST", f"/accounts/{ACCOUNT}/workers/observability/telemetry/query")])

    def test_account_id_comes_from_the_environment(self):
        transport = FakeTransport()
        code, _, _ = run_main(["--service", SERVICE, "--end", END], transport,
                              env={reader.ACCOUNT_ENV: ACCOUNT})
        self.assertEqual(code, 0)
        self.assertEqual([call[0] for call in transport.calls], ["POST"])

    def test_zero_or_several_accounts_never_guess(self):
        for accounts in ([], [{"id": ACCOUNT}, {"id": "f" * 32}]):
            code, out, err = run_main(["--service", SERVICE, "--end", END],
                                      FakeTransport(accounts=accounts))
            self.assertEqual((code, out), (1, ""))
            self.assertEqual(err.strip(), "container_stdout_read: read failed (account_not_unique)")

    def test_documented_query_still_matches_what_the_script_sends(self):
        doc = (ROOT / "docs" / "startup-diagnostics.md").read_text()
        block = doc.split("## Reading container stdout", 1)[1].split("```json", 1)[1].split("```", 1)[0]
        documented = json.loads(block.replace('"<uuid>"', '"q"').replace("<ms>", "0"))
        for key in ("view", "limit"):
            self.assertEqual(documented[key], REQUEST[key])
        self.assertEqual(documented["parameters"], {**REQUEST["parameters"], "filters": []})


class OutputTests(unittest.TestCase):
    def test_fixture_events_come_out_oldest_first_with_allowed_fields_only(self):
        packet = read_with(FakeTransport())
        self.assertEqual(packet["schema_version"], 1)
        self.assertEqual(packet["service"], SERVICE)
        self.assertEqual(packet["window"], {"start": "2026-10-11T12:15:00.000Z",
                                            "end": "2026-10-11T12:20:00.000Z"})
        self.assertEqual([e["ts"] for e in packet["events"]], [
            "2026-10-11T12:18:20.000Z", "2026-10-11T12:18:30.000Z",
            "2026-10-11T12:19:10.000Z", "2026-10-11T12:19:40.000Z"])
        failed = packet["events"][2]
        self.assertEqual(failed, {
            "ts": "2026-10-11T12:19:10.000Z", "level": "error",
            "message": "durable gateway failed; checkpoint unchanged, readiness unavailable",
            "msg": "durable_gateway_failed", "target": "two_bot::gateway",
            "run_id": "1791721100000000000-1", "error_class": "checkpoint_load_failed",
            "startup_phase": "durable_gateway"})
        self.assertNotIn("internal_detail", json.dumps(packet))
        self.assertNotIn("spans", json.dumps(packet))
        self.assertEqual(packet["events"][1]["code"], 8080)

    def test_ansi_and_control_characters_never_reach_the_reader(self):
        legacy = read_with(FakeTransport())["events"][0]
        self.assertEqual(legacy["message"], "2026-10-11T12:18:20Z legacy text line before JSON logging")
        answer = copy.deepcopy(RESPONSE)
        answer["result"]["events"]["events"] = [hostile_event("a\x1b]0;title\x07b\x00c\nd\x9be")]
        message = read_with(FakeTransport(answer))["events"][0]["message"]
        self.assertFalse(re.search(r"[\x00-\x1f\x7f-\x9f]", message), repr(message))

    def test_equal_timestamps_keep_oldest_first(self):
        newest_first = []
        for text in ("second", "first"):
            event = hostile_event(text)
            event["timestamp"] = 1791721190000
            newest_first.append(event)
        answer = copy.deepcopy(RESPONSE)
        answer["result"]["events"]["events"] = newest_first
        self.assertEqual([e["message"] for e in read_with(FakeTransport(answer))["events"]],
                         ["first", "second"])

    def test_playbook_literals_are_readable_in_the_output(self):
        messages = {e["message"] for e in read_with(FakeTransport())["events"]}
        self.assertIn("gateway ready; checkpoint committed", messages)
        self.assertIn("durable gateway failed; checkpoint unchanged, readiness unavailable", messages)

    def test_full_page_is_flagged_possibly_truncated(self):
        self.assertTrue(read_with(FakeTransport(), limit=4)["possibly_truncated"])
        self.assertFalse(read_with(FakeTransport(), limit=5)["possibly_truncated"])

    def test_empty_result_is_zero_events_not_a_failure(self):
        empty = copy.deepcopy(RESPONSE)
        empty["result"]["events"]["events"] = []
        code, out, _ = run_main(["--service", SERVICE, "--end", END], FakeTransport(empty), env={})
        self.assertEqual(code, 0)
        packet = json.loads(out)
        self.assertEqual((packet["event_count"], packet["events"]), (0, []))

    def test_cli_prints_one_json_document(self):
        code, out, err = run_main(["--service", SERVICE, "--end", END], FakeTransport())
        self.assertEqual((code, err), (0, ""))
        self.assertEqual(json.loads(out)["event_count"], 4)


class RedactionTests(unittest.TestCase):
    def packet_for(self, message, **source):
        answer = copy.deepcopy(RESPONSE)
        answer["result"]["events"]["events"] = [hostile_event(message, **source)]
        return read_with(FakeTransport(answer))

    def assert_hidden(self, packet, *secrets):
        text = json.dumps(packet)
        for secret in secrets:
            self.assertNotIn(secret, text)

    def test_every_secret_shape_is_replaced_in_the_message(self):
        message = " ".join(f"{label}:{HOSTILE[label]}" if label == "keyed" else HOSTILE[label]
                           for label in HOSTILE)
        packet = self.packet_for(message)
        self.assert_hidden(packet, "hunter2", "db.example.invalid", "q" * 24,
                           "plain-secret-value", "M" + "A" * 23, "z" * 27, "9" * 40, "9" * 18)
        self.assertGreaterEqual(packet["redactions"], 5)

    def test_structured_fields_are_redacted_too(self):
        packet = self.packet_for("x", error_class=HOSTILE["url"], step=HOSTILE["opaque"],
                                 cause="token=" + HOSTILE["opaque"], run_id=HOSTILE["opaque"])
        self.assert_hidden(packet, "hunter2", HOSTILE["opaque"])
        event = packet["events"][0]
        self.assertEqual(event["run_id"], "[redacted]")
        self.assertEqual(event["error_class"], "[redacted]")

    def test_persist_class_is_kept_and_the_free_action_label_is_not(self):
        packet = self.packet_for("internal action unknown-state persistence failed",
                                 msg="internal_action_unknown_state_persist_failed",
                                 persist_class="storage_unavailable", action="announcement.post")
        event = packet["events"][0]
        self.assertEqual(event["persist_class"], "storage_unavailable")
        self.assertNotIn("action", event)

    def test_terminal_persist_op_is_kept_and_the_free_action_label_is_not(self):
        for op in ("finish", "release"):
            with self.subTest(op=op):
                packet = self.packet_for("internal action terminal persistence failed",
                                         msg="internal_action_terminal_persist_failed",
                                         persist_op=op,
                                         persist_class="storage_unavailable",
                                         action="announcement.post")
                event = packet["events"][0]
                self.assertEqual(event["persist_op"], op)
                self.assertEqual(event["persist_class"], "storage_unavailable")
                self.assertNotIn("action", event)

    def test_bounded_message_type_is_kept_and_the_raw_header_is_not(self):
        packet = self.packet_for("eventsub delivery refused",
                                 msg="eventsub_delivery_refused",
                                 message_type="webhook_callback_verification",
                                 messageType="webhook_callback_verification")
        event = packet["events"][0]
        self.assertEqual(event["message_type"], "webhook_callback_verification")
        self.assertNotIn("messageType", event)

    def test_a_real_run_id_survives_but_a_snowflake_does_not(self):
        packet = self.packet_for("guild " + HOSTILE["snowflake"], run_id="1791721100000000000-7")
        event = packet["events"][0]
        self.assertEqual(event["run_id"], "1791721100000000000-7")
        self.assertEqual(event["message"], "guild [id]")

    def test_plain_snake_case_words_stay_readable(self):
        packet = self.packet_for("gateway_runtime_failed_after_checkpoint_commit_retry")
        self.assertEqual(packet["events"][0]["message"], "gateway_runtime_failed_after_checkpoint_commit_retry")
        self.assertEqual(packet["redactions"], 0)

    def test_english_log_text_is_not_mangled(self):
        # Literals the runbook tells operators to look for, plus scheme words in prose.
        for message in ("Discord refused the bot token; REST disabled until restart",
                        "create failed: bot lacks View/Send permission",
                        "Bearer authentication failed", "Basic checks passed",
                        "usage: two-bot guild-config-restore --snapshot FILE",
                        "gateway reconnect failed; Twilight will retry"):
            with self.subTest(message=message):
                packet = self.packet_for(message)
                self.assertEqual(packet["events"][0]["message"], message)
                self.assertEqual(packet["redactions"], 0)

    def test_credential_length_auth_values_are_replaced(self):
        for header in ("Authorization: Bearer " + "q" * 24, "Bot " + "Ab1" * 10, "Basic " + "dXNlcjpwYXNz" * 3):
            with self.subTest(header=header[:14]):
                packet = self.packet_for("sent " + header)
                self.assertNotRegex(json.dumps(packet), r"qqqq|Ab1Ab1|dXNlcjpwYXNz")
                self.assertGreaterEqual(packet["redactions"], 1)

    def test_keyed_auth_values_hide_the_credential_not_just_the_scheme(self):
        b64 = "dXNlcjpwYXNzd29yZDEyMzQ1Ng=="
        for fragment, secret in (("authorization: basic " + b64, b64),
                                 ("Authorization: BEARER " + "x9" * 15, "x9x9"),
                                 ("authorization: BOT " + "k7" * 15, "k7k7"),
                                 ("Authorization: Bearer shorttok", "shorttok"),
                                 ("sent bot " + "Ab1" * 10, "Ab1Ab1")):
            with self.subTest(fragment=fragment[:24]):
                packet = self.packet_for(fragment)
                self.assertNotIn(secret, packet["events"][0]["message"])
                self.assertGreaterEqual(packet["redactions"], 1)

    def test_long_key_names_and_escaped_quote_json_are_redacted(self):
        for fragment, secret in (("very_long_prefix_name_for_the_gateway_token=abc123", "abc123"),
                                 ("a" * 40 + "_token=hunter2", "hunter2"),
                                 ('{\\"token\\":\\"abc123\\"}', "abc123"),
                                 ('{\\"password\\": \\"hunter2\\"}', "hunter2")):
            with self.subTest(fragment=fragment[:30]):
                packet = self.packet_for("event " + fragment)
                self.assertNotIn(secret, packet["events"][0]["message"])
                self.assertGreaterEqual(packet["redactions"], 1)

    def test_json_quoted_key_names_are_redacted(self):
        for fragment, secret in (('{"token":"abc123"}', "abc123"), ('{"password": "hunter2"}', "hunter2"),
                                 ('"api_key":"short"', "short"), ("{'secret': 'tiny'}", "tiny"),
                                 ("DATABASE_DSN=x", "=x")):
            with self.subTest(fragment=fragment):
                packet = self.packet_for("event " + fragment)
                self.assertNotIn(secret, packet["events"][0]["message"])
                self.assertGreaterEqual(packet["redactions"], 1)

    def test_keyed_secret_containing_a_quote_is_fully_redacted(self):
        # A password containing a quote: plain JSON escapes it as \" and the
        # outer-escaped form (Rust Debug / embedded JSON) carries literal
        # backslashes before every quote. Either way no fragment past the
        # escaped quote may survive, in any backslash layering.
        inner = "ab\\\"cd efgh"
        plain = '{"password":"' + inner + '"}'
        escaped = plain.replace("\\", "\\\\").replace('"', '\\"')
        for fragment in (plain, escaped):
            with self.subTest(fragment=repr(fragment)):
                packet = self.packet_for("event " + fragment)
                message = packet["events"][0]["message"]
                for tail in ("cd efgh", "efgh", "ab"):
                    self.assertNotIn(tail, message.split("[redacted]")[-1])
                self.assertGreaterEqual(packet["redactions"], 1)
        single_plain = "{'secret': 'a\\'b cdefgh'}"
        packet = self.packet_for("event " + single_plain)
        self.assertNotIn("cdefgh", packet["events"][0]["message"])

    def test_a_runaway_line_is_bounded_in_time_and_size(self):
        for unit in ("a.", "a-", "a_1.", "token="):
            line = unit * 20000
            started = time.monotonic()
            packet = self.packet_for(line)
            self.assertLess(time.monotonic() - started, 5.0, unit)
            self.assertLessEqual(len(packet["events"][0]["message"]), reader.MAX_VALUE_CHARS + 3)
        self.assertEqual(self.packet_for("a" * 5000)["events"][0]["message"], "...")

    def test_redaction_never_sees_more_than_the_bound(self):
        seen = []
        real = reader.redact

        def spy(text):
            seen.append(len(text))
            return real(text)

        with mock.patch.object(reader, "redact", spy):
            self.packet_for("a-" * 20000)
        self.assertEqual(len(seen), 2)  # the level, then the message
        self.assertLessEqual(max(seen), reader.MAX_REDACT_CHARS)

    def test_a_secret_cut_by_the_redaction_bound_leaves_no_fragment(self):
        fragment = "A1" * 30
        line = "y " * (reader.MAX_REDACT_CHARS // 2 - 1) + fragment + " tail"
        message = self.packet_for(line)["events"][0]["message"]
        self.assertNotIn("A1A1", message)
        self.assertTrue(message.endswith("..."))

    def test_nested_and_non_scalar_fields_are_dropped(self):
        packet = self.packet_for("x", error_class={"nested": HOSTILE["url"]}, step=["a"], job=None)
        event = packet["events"][0]
        self.assertEqual(set(event), {"ts", "level", "message"})

    def test_long_values_are_cut(self):
        packet = self.packet_for("a " * 400)
        self.assertLessEqual(len(packet["events"][0]["message"]), reader.MAX_VALUE_CHARS + 3)


class FenceTests(unittest.TestCase):
    def refuse(self, argv, env=None):
        transport = FakeTransport()
        code, out, err = run_main(argv, transport, env=env)
        self.assertEqual((code, out), (2, ""), argv)
        self.assertEqual(transport.calls, [], argv)
        self.assertEqual(err.count("\n"), 1, argv)
        return err

    def test_bad_arguments_refuse_before_anything_is_sent(self):
        base = ["--service", SERVICE, "--end", END]
        for argv in (
            ["--service", "not-a-uuid"],
            ["--service", "ABCDEF01-2222-4333-8444-555555555555"],
            [*base, "--minutes", "0"],
            [*base, "--minutes", str(reader.MAX_MINUTES + 1)],
            [*base, "--limit", "0"],
            [*base, "--limit", str(reader.MAX_LIMIT + 1)],
            [*base, "--contains", "bad\nline"],
            [*base, "--contains", "x" * (reader.MAX_CONTAINS + 1)],
            [*base, "--contains", ""],
            ["--service", SERVICE, "--end", "2026-10-11 12:20:00"],
            ["--service", SERVICE, "--end", "2026-13-40T99:00:00Z"],
            ["--service", SERVICE, "--end", "2026-10-11T12:30:01Z"],  # one second ahead of NOW
            [*base, "--account-id", "short"],
        ):
            with self.subTest(argv=argv):
                self.refuse(argv)

    def test_a_malformed_environment_account_refuses(self):
        self.refuse(["--service", SERVICE, "--end", END], env={reader.ACCOUNT_ENV: "nope"})

    def test_missing_or_malformed_token_refuses_without_a_transport(self):
        for env in ({}, {reader.TOKEN_ENV: ""}, {reader.TOKEN_ENV: "short"},
                    {reader.TOKEN_ENV: "has a space " * 3}, {reader.TOKEN_ENV: "a" * 201}):
            code, out, err = run_main(["--service", SERVICE, "--end", END], env=env)
            self.assertEqual((code, out), (2, ""), env)
            self.assertIn(reader.TOKEN_ENV, err)
            self.assertNotIn("short", err.replace("is not set to a read-only API token", ""))

    def test_unknown_flag_exits_two_through_argparse(self):
        run = subprocess.run([sys.executable, str(SCRIPT), "--service", SERVICE, "--write"],
                             capture_output=True, text=True, env={"PATH": os.environ.get("PATH", "")})
        self.assertEqual(run.returncode, 2)
        self.assertEqual(run.stdout, "")

    def test_help_exits_zero(self):
        run = subprocess.run([sys.executable, str(SCRIPT), "--help"], capture_output=True, text=True,
                             env={"PATH": os.environ.get("PATH", "")})
        self.assertEqual(run.returncode, 0)
        self.assertIn("--service", run.stdout)

    def test_end_of_now_is_allowed(self):
        # The fixture events are 12:18-12:19, so a 15-minute window ending at
        # now (12:15-12:30) keeps them in-window while still pinning end==now.
        code, _, _ = run_main(["--service", SERVICE, "--end", "2026-10-11T12:30:00Z",
                               "--minutes", "15"], FakeTransport(),
                              env={reader.ACCOUNT_ENV: ACCOUNT})
        self.assertEqual(code, 0)


class FailureTests(unittest.TestCase):
    def test_malformed_responses_fail_closed_with_a_fixed_line(self):
        events = RESPONSE["result"]["events"]["events"]
        broken = [
            [], None, "text", 5,
            {"success": False, "result": RESPONSE["result"]},
            {"success": True, "result": None},
            {"success": True, "result": {"events": {"events": "nope"}}},
            {"success": True, "result": {"events": {"events": ["x"]}}},
            {"success": True, "result": {"events": {"events": [{"timestamp": "late", "source": {}}]}}},
            {"success": True, "result": {"events": {"events": [{**events[0], "timestamp": 10 ** 30}]}}},
        ]
        for answer in broken:
            with self.subTest(answer=str(answer)[:60]):
                code, out, err = run_main(["--service", SERVICE, "--end", END], FakeTransport(answer),
                                          env={reader.ACCOUNT_ENV: ACCOUNT})
                self.assertEqual((code, out), (1, ""))
                self.assertEqual(err.strip(), "container_stdout_read: read failed (bad_response)")


class _Recorder(http.server.BaseHTTPRequestHandler):
    """Loopback stand-in for api.cloudflare.com; behaviour is set on the server."""

    def log_message(self, *args):
        pass

    def _handle(self):
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length) if length else b""
        self.server.seen.append((self.command, self.path, dict(self.headers), body))
        status, payload, extra = self.server.reply(self)
        raw = payload if isinstance(payload, bytes) else json.dumps(payload).encode()
        self.send_response(status)
        for name, value in extra.items():
            self.send_header(name, value)
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    do_GET = do_POST = do_PUT = do_DELETE = do_PATCH = _handle


class LoopbackTests(unittest.TestCase):
    def setUp(self):
        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), _Recorder)
        self.server.seen = []
        self.server.reply = lambda handler: (200, RESPONSE, {})
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.addCleanup(self.server.server_close)
        self.addCleanup(self.server.shutdown)
        self.base = f"http://127.0.0.1:{self.server.server_address[1]}"
        self.transport = reader.HttpTransport(TOKEN, base=self.base, timeout=5)
        self.query_path = f"/accounts/{ACCOUNT}/workers/observability/telemetry/query"

    def test_wire_request_is_the_pinned_read_query(self):
        answer = self.transport("POST", self.query_path, REQUEST)
        self.assertEqual(answer, RESPONSE)
        [(method, path, headers, body)] = self.server.seen
        self.assertEqual((method, path), ("POST", self.query_path))
        self.assertEqual(json.loads(body), REQUEST)
        lowered = {name.lower(): value for name, value in headers.items()}
        self.assertEqual(lowered["authorization"], f"Bearer {TOKEN}")
        self.assertEqual(lowered["user-agent"], reader.USER_AGENT)
        self.assertEqual(lowered["content-type"], "application/json")
        self.assertNotIn("Python-urllib", lowered["user-agent"])

    def test_accounts_list_is_a_bare_get(self):
        self.server.reply = lambda handler: (200, {"success": True, "result": [{"id": ACCOUNT}]}, {})
        self.assertEqual(self.transport("GET", "/accounts")["result"][0]["id"], ACCOUNT)
        [(method, path, headers, body)] = self.server.seen
        self.assertEqual((method, path, body), ("GET", "/accounts", b""))
        self.assertNotIn("content-type", {name.lower() for name in headers})

    def test_nothing_but_the_two_reads_can_be_sent(self):
        for method, path, body in (
            ("PUT", self.query_path, REQUEST),
            ("DELETE", self.query_path, None),
            ("PATCH", "/accounts", {}),
            ("POST", "/accounts", {}),
            ("GET", "/user/tokens/verify", None),
            ("GET", f"/accounts/{ACCOUNT}/workers/scripts", None),
            ("POST", f"/accounts/{ACCOUNT}/workers/observability/telemetry/query/../keys", REQUEST),
            ("POST", "/accounts/short/workers/observability/telemetry/query", REQUEST),
            ("GET", "/accounts", {"x": 1}),
        ):
            with self.subTest(method=method, path=path):
                with self.assertRaises(reader.Refused):
                    self.transport(method, path, body)
        self.assertEqual(self.server.seen, [])

    def test_http_errors_become_fixed_classes_and_never_echo_the_body(self):
        leak = "LEAK-" + HOSTILE["opaque"]
        for status, name in ((401, "unauthorized"), (403, "unauthorized"), (429, "rate_limited"),
                             (400, "bad_request"), (404, "bad_request"), (502, "server_error")):
            self.server.reply = lambda handler, status=status: (status, {"error": leak, "token": TOKEN}, {})
            code, out, err = run_main(["--service", SERVICE, "--end", END], self.transport,
                                      env={reader.ACCOUNT_ENV: ACCOUNT})
            self.assertEqual((code, out), (1, ""), status)
            self.assertEqual(err.strip(), f"container_stdout_read: read failed ({name})")
            self.assertNotIn(leak, err)
            self.assertNotIn(TOKEN, err)

    def test_redirect_is_not_followed_and_the_token_goes_nowhere_else(self):
        other = http.server.ThreadingHTTPServer(("127.0.0.1", 0), _Recorder)
        other.seen = []
        other.reply = lambda handler: (200, RESPONSE, {})
        threading.Thread(target=other.serve_forever, daemon=True).start()
        self.addCleanup(other.server_close)
        self.addCleanup(other.shutdown)
        target = f"http://127.0.0.1:{other.server_address[1]}/elsewhere"
        self.server.reply = lambda handler: (302, b"", {"Location": target})
        with self.assertRaises(reader.ReadFailed):
            self.transport("POST", self.query_path, REQUEST)
        self.assertEqual(other.seen, [])

    def test_a_malformed_http_answer_is_a_network_error_not_a_traceback(self):
        listener = socket.socket()
        listener.bind(("127.0.0.1", 0))
        listener.listen(1)
        self.addCleanup(listener.close)

        def answer_garbage():
            connection, _ = listener.accept()
            with connection:
                connection.recv(65536)
                connection.sendall(b"this is not http\r\n\r\n")

        threading.Thread(target=answer_garbage, daemon=True).start()
        broken = reader.HttpTransport(TOKEN, base=f"http://127.0.0.1:{listener.getsockname()[1]}", timeout=5)
        out, err = io.StringIO(), io.StringIO()
        code = reader.main(["--service", SERVICE, "--end", END, "--account-id", ACCOUNT], env={},
                           transport=broken, now=lambda: NOW, stdout=out, stderr=err)
        self.assertEqual((code, out.getvalue()), (1, ""))
        self.assertEqual(err.getvalue().strip(), "container_stdout_read: read failed (network_error)")

    def test_unreachable_host_is_a_network_error(self):
        dead = reader.HttpTransport(TOKEN, base="http://127.0.0.1:9", timeout=2)
        with self.assertRaises(reader.ReadFailed) as caught:
            dead("POST", self.query_path, REQUEST)
        self.assertEqual(str(caught.exception), "network_error")

    def test_oversized_and_non_json_answers_are_bad_responses(self):
        for payload in (b"x" * (reader.MAX_RESPONSE_BYTES + 1), b"<html>not json</html>", b"[1,"):
            self.server.reply = lambda handler, payload=payload: (200, payload, {})
            with self.assertRaises(reader.ReadFailed) as caught:
                self.transport("POST", self.query_path, REQUEST)
            self.assertEqual(str(caught.exception), "bad_response")

    def test_end_to_end_through_main_against_loopback(self):
        out, err = io.StringIO(), io.StringIO()
        code = reader.main(["--service", SERVICE, "--end", END, "--account-id", ACCOUNT],
                           env={}, transport=self.transport, now=lambda: NOW, stdout=out, stderr=err)
        self.assertEqual((code, err.getvalue()), (0, ""))
        self.assertEqual(json.loads(out.getvalue())["event_count"], 4)
        self.assertEqual([seen[0] for seen in self.server.seen], ["POST"])
        self.assertNotIn(TOKEN, out.getvalue())


class DocTests(unittest.TestCase):
    def test_the_diagnostics_doc_links_the_script(self):
        doc = (ROOT / "docs" / "startup-diagnostics.md").read_text()
        section = doc.split("## Reading container stdout", 1)[1].split("\n## ", 1)[0]
        self.assertIn("scripts/container_stdout_read.py", section)

    def test_no_doc_still_says_the_api_cannot_reach_container_stdout(self):
        stale = re.compile(r"No Cloudflare API reaches container stdout|container stdout is dashboard-only"
                           r"|Container stdout is dashboard-only", re.IGNORECASE)
        offenders = [str(path.relative_to(ROOT)) for path in sorted((ROOT / "docs").glob("*.md"))
                     if stale.search(path.read_text())]
        self.assertEqual(offenders, [])


if __name__ == "__main__":
    unittest.main()
