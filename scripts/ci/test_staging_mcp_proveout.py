"""Offline prove-out guards; never touches staging.

do_post must not follow 301/302/303 redirects: the Authorization bearer
must stay on STAGING_MCP_URL and never reach a Location target.
The URL fence allows only the exact staging https /mcp URL, and every
response body read is capped at MAX_BODY_BYTES (over-cap raises
BodyTooLarge before decoding).
"""

import io
import json
import os
import sys
import tempfile
import threading
import unittest
import urllib.error
from contextlib import redirect_stdout
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

import staging_mcp_proveout as proveout


def shipped_tools():
    """Fresh tools/list fixture from wrangler/src/mcp.ts toolEntries()."""
    fixture = ROOT / "scripts/ci/fixtures/staging_mcp_shipped_contract.json"
    return json.loads(fixture.read_text())["tools_result"]["tools"]


class _Target(BaseHTTPRequestHandler):
    hits = []

    def do_GET(self):
        _Target.hits.append(
            (self.headers.get("Authorization"), self.path)
        )
        body = b"ok"
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


class _Redirector(BaseHTTPRequestHandler):
    target_port = 0
    code = 302

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0") or "0")
        if length:
            self.rfile.read(length)
        self.send_response(type(self).code)
        self.send_header(
            "Location", f"http://127.0.0.1:{type(self).target_port}/elsewhere"
        )
        self.end_headers()

    def log_message(self, *args):
        pass


class RedirectGuardTests(unittest.TestCase):
    def _run_code(self, code):
        _Target.hits.clear()
        target = ThreadingHTTPServer(("127.0.0.1", 0), _Target)
        redirector = ThreadingHTTPServer(("127.0.0.1", 0), _Redirector)
        _Redirector.target_port = target.server_port
        _Redirector.code = code
        t1 = threading.Thread(target=target.serve_forever, daemon=True)
        t2 = threading.Thread(target=redirector.serve_forever, daemon=True)
        t1.start()
        t2.start()
        try:
            url = f"http://127.0.0.1:{redirector.server_port}/mcp"
            status, headers, body = proveout.do_post(
                url, {"jsonrpc": "2.0", "id": 1, "method": "tools/list"},
                "test-bearer-000000000000000000000000",
                timeout=5,
            )
            return status, headers, body, list(_Target.hits)
        finally:
            target.shutdown()
            redirector.shutdown()
            t1.join(timeout=5)
            t2.join(timeout=5)
            target.server_close()
            redirector.server_close()

    def test_302_does_not_forward_bearer(self):
        status, _headers, body, hits = self._run_code(302)
        self.assertEqual(status, 302)
        self.assertEqual(hits, [])
        self.assertNotIn("test-bearer", body)

    def test_301_and_303_do_not_forward_bearer(self):
        for code in (301, 303):
            with self.subTest(code=code):
                status, _headers, body, hits = self._run_code(code)
                self.assertEqual(status, code)
                self.assertEqual(hits, [])
                self.assertNotIn("test-bearer", body)


class UrlFenceTests(unittest.TestCase):
    def test_default_and_worker_urls_pass(self):
        self.assertTrue(proveout.valid_staging_mcp_url(proveout.DEFAULT_URL))
        self.assertTrue(proveout.valid_staging_mcp_url(
            "https://two-bot-next-staging.other-sub.workers.dev/mcp"))

    def test_suffix_userinfo_port_path_query_fragment_refuse(self):
        base = "https://two-bot-next-staging.5150.workers.dev"
        evil = (
            "https://two-bot-next-staging.evil.example/mcp",
            f"{base}.evil.example/mcp",
            "https://two-bot-next-staging@evil.example/mcp",
            "https://user@two-bot-next-staging.5150.workers.dev/mcp",
            "https://@two-bot-next-staging.5150.workers.dev/mcp",
            "https://:@two-bot-next-staging.5150.workers.dev/mcp",
            f"{base}:8443/mcp",
            base + "/mcp/extra",
            base + "/ops/mcp",
            base + "/MCP",
            base + "/mcp?x=1",
            base + "/mcp#x",
            base + "/",
            base,
            "http://two-bot-next-staging.5150.workers.dev/mcp",
            "https://two-bot-next-production.5150.workers.dev/mcp",
            "",
            None,
        )
        for url in evil:
            with self.subTest(url=url):
                self.assertFalse(proveout.valid_staging_mcp_url(url))


class MainFenceTests(unittest.TestCase):
    def test_refused_url_exits_2_without_probes_or_artifact(self):
        token = "test-bearer-000000000000000000000000"
        with tempfile.TemporaryDirectory() as tmp:
            out = str(Path(tmp) / "proveout.json")
            argv = ["staging_mcp_proveout.py", "--output", out]
            env = {
                proveout.TOKEN_ENV: token,
                proveout.URL_ENV: "https://two-bot-next-staging.evil.example/mcp",
            }
            with mock.patch.object(sys, "argv", argv), \
                    mock.patch.dict(os.environ, env), \
                    mock.patch("urllib.request.build_opener") as build_opener, \
                    mock.patch.object(proveout, "do_post") as do_post:
                self.assertEqual(proveout.main(), 2)
            build_opener.assert_not_called()
            do_post.assert_not_called()
            self.assertFalse(Path(out).exists())


class AuditContractTests(unittest.TestCase):
    def run_proveout(self, audit_reply):
        token = "test-bearer-000000000000000000000000"

        def reply(_url, payload, bearer):
            envelope = {"jsonrpc": "2.0", "id": payload["id"]}
            if bearer != token:
                return 401, {"www-authenticate": "Bearer"}, "{}"
            if payload["method"] == "initialize":
                envelope["result"] = {"protocolVersion": "2025-06-18"}
            elif payload["method"] == "tools/list":
                envelope["result"] = {"tools": shipped_tools()}
            else:
                tool = payload["params"]["name"]
                if tool == "get_audit":
                    envelope.update(audit_reply)
                elif tool == "no_such_tool":
                    envelope["error"] = {"code": -32601, "message": "unknown tool"}
                else:
                    envelope["result"] = {"content": []}
            return 200, {}, json.dumps(envelope)

        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")) as tmp:
            out = str(Path(tmp) / "proveout.json")
            with mock.patch.object(sys, "argv", ["proveout", "--output", out]), \
                    mock.patch.dict(os.environ, {proveout.TOKEN_ENV: token,
                                                proveout.URL_ENV: proveout.DEFAULT_URL}), \
                    mock.patch.object(proveout, "do_post", side_effect=reply) as do_post, \
                    mock.patch("sys.stdout", new_callable=io.StringIO) as stdout:
                code = proveout.main()
            self.assertEqual(do_post.call_count, 10)
            self.assertNotIn(token, stdout.getvalue())
            artifact_text = Path(out).read_text()
            self.assertNotIn(token, artifact_text)
            return code, json.loads(artifact_text)["failures"]

    def test_real_available_and_unavailable_excerpts_pass(self):
        for excerpt in (
            {"available": True, "rows": [], "truncated": False},
            {"available": False, "rows": [], "truncated": False},
            {"available": True, "rows": [{"entry_id": "synthetic-entry"}], "truncated": False},
            {"available": True, "rows": [{}] * 50, "truncated": True},
        ):
            with self.subTest(excerpt=excerpt):
                self.assertEqual(self.run_proveout({"result": {
                    "content": [], "structuredContent": excerpt,
                }}), (0, []))

    def test_audit_container_failure_is_expected_while_fenced(self):
        with mock.patch.dict(os.environ, {proveout.EXPECT_FENCE_ENV: "1"}):
            self.assertEqual(self.run_proveout({"error": {
                "code": -32000, "message": "container request failed",
            }}), (0, []))

    def test_other_audit_errors_still_fail(self):
        code, failures = self.run_proveout({"error": {"code": -32603, "message": "x"}})
        self.assertEqual(code, 1)
        self.assertEqual(failures, ["tools/call get_audit: unexpected JSON-RPC error code -32603"])

    def test_stub_and_malformed_audit_envelopes_fail(self):
        for excerpt in (
            {"implemented": False}, None, [],
            {"available": True, "rows": [], "truncated": False, "extra": 1},
            {"available": 1, "rows": [], "truncated": False},
            {"available": True, "rows": None, "truncated": False},
            {"available": True, "rows": ["not-a-row"], "truncated": False},
            {"available": True, "rows": [{}] * 51, "truncated": True},
            {"available": True, "rows": [], "truncated": 0},
            {"available": False, "rows": [{}], "truncated": False},
            {"available": False, "rows": [], "truncated": True},
        ):
            with self.subTest(excerpt=excerpt):
                code, failures = self.run_proveout({"result": {
                    "content": [], "structuredContent": excerpt,
                }})
                self.assertEqual(code, 1)
                self.assertEqual(failures, ["tools/call get_audit: expected bounded audit excerpt envelope"])


class BodyCapTests(unittest.TestCase):
    TOKEN = "test-bearer-000000000000000000000000"
    URL = "https://two-bot-next-staging.5150.workers.dev/mcp"

    def response(self, payload, seen, status=200):
        class _Resp:
            def read(self, n):
                seen.append(n)
                return payload

            def __enter__(self):
                return self

            def __exit__(self, *exc):
                return False

            @property
            def headers(self):
                return {"Content-Type": "application/json"}

        resp = _Resp()
        resp.status = status
        return resp

    def http_error(self, payload, seen, code=500):
        err = urllib.error.HTTPError(self.URL, code, "Server Error", {}, io.BytesIO(b""))
        err.read = lambda n, _payload=payload: (seen.append(n), _payload)[1]
        return err

    def test_cap_matches_the_worker_bound(self):
        self.assertEqual(proveout.MAX_BODY_BYTES, 64 * 1024)

    def test_exact_cap_valid_input_passes(self):
        seen = []
        body = b'{"result":{}}' + b" " * (proveout.MAX_BODY_BYTES - len(b'{"result":{}}'))
        self.assertEqual(len(body), proveout.MAX_BODY_BYTES)
        opener = mock.Mock()
        opener.open.return_value = self.response(body, seen)
        with mock.patch("urllib.request.build_opener", return_value=opener):
            status, _headers, text = proveout.do_post(self.URL, {"id": 1}, self.TOKEN)
        self.assertEqual(status, 200)
        self.assertEqual(text, body.decode("utf-8"))
        self.assertEqual(seen, [proveout.MAX_BODY_BYTES + 1])

    def test_oversize_body_refuses_with_fixed_message(self):
        sentinel = "SENTINEL_BODY_mcp_proveout_padded"
        base = f'{{"result":{{"note":"{sentinel}"}}}}'.encode()
        body = base + b" " * (proveout.MAX_BODY_BYTES + 1 - len(base))
        self.assertEqual(len(body), proveout.MAX_BODY_BYTES + 1)
        seen = []
        opener = mock.Mock()
        opener.open.return_value = self.response(body, seen)
        with mock.patch("urllib.request.build_opener", return_value=opener):
            with self.assertRaises(proveout.BodyTooLarge) as caught:
                proveout.do_post(self.URL, {"id": 1}, self.TOKEN)
        message = str(caught.exception)
        self.assertIn("over the size cap", message)
        self.assertNotIn(sentinel, message)
        self.assertNotIn(self.TOKEN, message)
        self.assertEqual(seen, [proveout.MAX_BODY_BYTES + 1])

    def test_oversize_http_error_body_refuses_the_same_way(self):
        sentinel = "SENTINEL_BODY_mcp_proveout_error_padded"
        body = sentinel.encode() + b"y" * (proveout.MAX_BODY_BYTES + 1 - len(sentinel))
        self.assertEqual(len(body), proveout.MAX_BODY_BYTES + 1)
        seen = []
        opener = mock.Mock()
        opener.open.side_effect = self.http_error(body, seen)
        with mock.patch("urllib.request.build_opener", return_value=opener):
            with self.assertRaises(proveout.BodyTooLarge) as caught:
                proveout.do_post(self.URL, {"id": 1}, self.TOKEN)
        message = str(caught.exception)
        self.assertIn("over the size cap", message)
        self.assertNotIn(sentinel, message)
        self.assertNotIn(self.TOKEN, message)
        self.assertEqual(seen, [proveout.MAX_BODY_BYTES + 1])


class HostileBodyTests(unittest.TestCase):
    TOKEN = "test-bearer-000000000000000000000000"
    PROBES = (["initialize", "tools/list"]
              + [f"tools/call {tool}" for tool in proveout.EXPECTED_TOOLS]
              + ["tools/call unknown", "wrong bearer", "no bearer"])

    def run_main(self, overrides=None):
        # Baseline envelopes are bound to their requests (jsonrpc 2.0 with
        # the probe-order numeric id), so overrides exercise shape handling
        # rather than envelope binding.
        responses = [
            (200, {}, json.dumps({"jsonrpc": "2.0", "id": 1, "result": {
                "protocolVersion": "2025-06-18"}})),
            (200, {}, json.dumps({"jsonrpc": "2.0", "id": 2, "result": {"tools": shipped_tools()}})),
        ] + [(200, {}, json.dumps({"jsonrpc": "2.0", "id": 3 + offset, "result": {
            "content": [], "structuredContent": {
                "available": True, "rows": [], "truncated": False}}}))
             for offset in range(len(proveout.EXPECTED_TOOLS))] + [
            (200, {}, '{"jsonrpc":"2.0","id":8,'
             '"error":{"code":-32601,"message":"unknown tool"}}'),
            (401, {"www-authenticate": "Bearer"}, "{}"),
            (401, {"www-authenticate": "Bearer"}, "{}"),
        ]
        for index, response in (overrides or {}).items():
            responses[index] = response
        with tempfile.TemporaryDirectory(
                dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")) as tmp:
            output = Path(tmp) / "receipt.json"
            argv = ["staging_mcp_proveout.py", "--output", str(output)]
            env = {proveout.TOKEN_ENV: self.TOKEN,
                   proveout.URL_ENV: proveout.DEFAULT_URL}
            raw_stdout = io.BytesIO()
            stdout = io.TextIOWrapper(raw_stdout, encoding="utf-8", errors="strict",
                                      write_through=True)
            with mock.patch.object(proveout, "do_post", side_effect=responses) as post, \
                    mock.patch.object(sys, "argv", argv), \
                    mock.patch.dict(os.environ, env), redirect_stdout(stdout):
                code = proveout.main()
            raw = output.read_bytes()
            artifact = json.loads(raw)
        self.assertEqual(post.call_count, 10)
        self.assertEqual([p["name"] for p in artifact["probes"]], self.PROBES)
        self.assertEqual(artifact["schema_version"], proveout.SCHEMA_VERSION)
        text = raw_stdout.getvalue().decode("utf-8")
        self.assertNotIn(self.TOKEN, text)
        self.assertNotIn(self.TOKEN.encode(), raw)
        return code, text, artifact, raw

    def assert_shape_failure(self, index, body):
        code, stdout, artifact, raw = self.run_main({index: (200, {}, body)})
        self.assertEqual(code, 1)
        self.assertEqual(len(artifact["failures"]), 1)
        self.assertTrue(artifact["failures"][0].startswith(self.PROBES[index] + ":"))
        self.assertNotIn("body cap", artifact["failures"][0])
        self.assertLess(len(stdout.encode()), proveout.MAX_BODY_BYTES)
        self.assertLess(len(raw), proveout.MAX_BODY_BYTES)
        return artifact

    def test_valid_script_and_fenced_container_errors(self):
        code, _, artifact, _ = self.run_main({})
        self.assertEqual(code, 0)
        self.assertEqual(artifact["failures"], [])
        # Corroborated fence: bound -32000 envelopes plus the explicit
        # dispatcher expectation still pass.
        overrides = {i: (200, {}, json.dumps(
            {"jsonrpc": "2.0", "id": i + 1, "error": {
                "code": -32000, "message": "container request failed"}}))
            for i in (2, 3, 4)}
        with mock.patch.dict(os.environ, {proveout.EXPECT_FENCE_ENV: "1"}):
            code, _, artifact, _ = self.run_main(overrides)
        self.assertEqual(code, 0)
        self.assertEqual(artifact["failures"], [])

    def test_mid_depth_json_falls_back_before_serialization(self):
        for depth in (900, 2000):
            with self.subTest(depth=depth):
                body = "[" * depth + "]" * depth
                artifact = self.assert_shape_failure(0, body)
                self.assertEqual(artifact["probes"][0]["body"], {"_text": body})

    def test_json_depth_limit_handles_arrays_and_objects(self):
        for opener, closer in (("[", "]"), ('{"a":', "}")):
            for depth in (32, 33):
                with self.subTest(opener=opener, depth=depth):
                    body = opener * depth + "0" + closer * depth
                    expected = json.loads(body) if depth == 32 else {"_text": body}
                    self.assertEqual(proveout.parse_json_or_text(body), expected)

    def test_wide_nested_body_does_not_expand_with_indentation(self):
        body = "[" * 30 + ",".join("0" for _ in range(10000)) + "]" * 30
        artifact = self.assert_shape_failure(0, body)
        self.assertEqual(artifact["probes"][0]["body"], json.loads(body))

    def test_wrong_typed_result_and_tool_entries_are_shape_failures(self):
        bodies = [
            {"result": ["x"]}, {"result": "x"}, {"result": True},
            {"result": {"tools": ["x"]}},
            {"result": {"tools": [{"name": "get_status"}, {"name": 1}]}},
            {"result": {"tools": [{"name": None}]}},
            {"result": {"tools": [{}]}},
        ]
        for body in bodies:
            with self.subTest(body=body):
                body = {"jsonrpc": "2.0", "id": 2, **body}
                self.assert_shape_failure(1, json.dumps(body))

    def test_wrong_typed_errors_are_shape_failures(self):
        for index in range(2, 8):
            for error in ("x", ["x"], True, 1):
                with self.subTest(index=index, error=error):
                    self.assert_shape_failure(index, json.dumps({
                        "jsonrpc": "2.0", "id": index + 1, "error": error}))

    def test_non_integer_error_codes_are_shape_failures(self):
        for index in range(2, 8):
            for code in ("\ud800x", "-32000", -32000.0, -32601.0, True, ["x"]):
                with self.subTest(index=index, code=code):
                    self.assert_shape_failure(index, json.dumps({
                        "jsonrpc": "2.0", "id": index + 1,
                        "error": {"code": code}}))

    def test_non_200_authenticated_replies_name_the_http_status(self):
        # One case per authenticated stage family: transport and credential
        # failures must report the HTTP status, never an envelope code.
        transports = [
            (401, {}, "unauthorized"),
            (503, {}, "<html>busy</html>"),
            (None, {}, "network-error"),
        ]
        for index in (0, 1, 2, 7):
            for status, headers, body in transports:
                with self.subTest(stage=self.PROBES[index], status=status):
                    code, _stdout, artifact, _raw = self.run_main({
                        index: (status, headers, body)})
                    self.assertEqual(code, 1)
                    self.assertEqual(len(artifact["failures"]), 1)
                    failure = artifact["failures"][0]
                    self.assertTrue(
                        failure.startswith(self.PROBES[index] + ":"))
                    self.assertIn(f"HTTP {status}", failure)
                    self.assertNotIn("ENVELOPE", failure)
                    self.assertEqual(
                        artifact["proof_disposition"], "failed")

    def test_error_without_string_message_fails_framing(self):
        errors = [
            {"code": -32601},
            {"code": -32601, "message": None},
            {"code": -32601, "message": 123},
            {"code": -32601, "message": True},
            {"code": -32601, "message": ["x"]},
        ]
        for index in (0, 2, 7):
            for error in errors:
                with self.subTest(stage=self.PROBES[index], error=error):
                    code, _stdout, artifact, _raw = self.run_main({
                        index: (200, {}, json.dumps({
                            "jsonrpc": "2.0", "id": index + 1,
                            "error": error}))})
                    self.assertEqual(code, 1)
                    self.assertEqual(len(artifact["failures"]), 1)
                    self.assertIn("[ENVELOPE_FRAMING]", artifact["failures"][0])

    def test_message_less_32000_is_not_a_corroborated_fence(self):
        body = json.dumps({"jsonrpc": "2.0", "id": 3, "error": {"code": -32000}})
        with mock.patch.dict(os.environ, {proveout.EXPECT_FENCE_ENV: "1"}):
            code, _stdout, artifact, _raw = self.run_main({2: (200, {}, body)})
        self.assertEqual(code, 1)
        self.assertEqual(len(artifact["failures"]), 1)
        self.assertIn("[ENVELOPE_FRAMING]", artifact["failures"][0])

    def test_non_boolean_is_error_fails_content(self):
        for is_error in (1, "true", "false", 0):
            with self.subTest(isError=is_error):
                code, _stdout, artifact, _raw = self.run_main({
                    2: (200, {}, json.dumps({
                        "jsonrpc": "2.0", "id": 3, "result": {
                            "content": [{"type": "text", "text": "ok"}],
                            "isError": is_error}}))})
                self.assertEqual(code, 1)
                self.assertEqual(len(artifact["failures"]), 1)
                self.assertIn("[CONTENT_INVALID]", artifact["failures"][0])

    def test_explicit_is_error_false_still_passes(self):
        code, _stdout, artifact, _raw = self.run_main({
            2: (200, {}, json.dumps({
                "jsonrpc": "2.0", "id": 3, "result": {
                    "content": [{"type": "text", "text": "ok"}],
                    "isError": False}}))})
        self.assertEqual(code, 0)
        self.assertEqual(artifact["failures"], [])

    def test_wrong_typed_audit_structured_content_is_shape_failure(self):
        for structured in (["x"], "x", True, 1):
            with self.subTest(structured=structured):
                self.assert_shape_failure(6, json.dumps({
                    "jsonrpc": "2.0", "id": 7, "result": {
                        "content": [],
                        "structuredContent": structured}}))

    def test_top_level_json_values_fail_envelope_probes_without_crashing(self):
        for index in range(8):
            for body in ("null", "true", "1", '"x"', "[]"):
                with self.subTest(index=index, body=body):
                    self.assert_shape_failure(index, body)

    def test_cap_refusal_is_mapped_for_every_probe(self):
        for index, name in enumerate(self.PROBES):
            with self.subTest(probe=name):
                code, stdout, artifact, raw = self.run_main({
                    index: proveout.BodyTooLarge("response body over the size cap")})
                self.assertEqual(code, 1)
                self.assertEqual(artifact["failures"], [f"{name}: answered over the body cap"])
                self.assertEqual(artifact["probes"][index]["body"], {"_text": ""})
                self.assertLess(len(stdout.encode()), proveout.MAX_BODY_BYTES)
                self.assertLess(len(raw), proveout.MAX_BODY_BYTES)


class OutputSafetyTests(unittest.TestCase):
    def test_reflected_bearer_is_redacted_from_logs_artifact_and_failures(self):
        token = HostileBodyTests.TOKEN
        code, stdout, artifact, raw = HostileBodyTests().run_main({
            0: (200, {"www-authenticate": f"Bearer {token}"}, json.dumps({
                "result": {"protocolVersion": token},
            })),
            1: (200, {}, json.dumps({
                "result": {"tools": [{"name": token}]},
            })),
            7: (200, {}, json.dumps({
                "error": {"code": -32000, "message": token},
            })),
        })
        self.assertEqual(code, 1)
        self.assertNotIn(token, stdout)
        self.assertNotIn(token.encode(), raw)
        self.assertNotIn(token, json.dumps(artifact))
        self.assertEqual(
            artifact["probes"][0]["body"]["result"]["protocolVersion"], "***"
        )
        self.assertEqual(
            artifact["probes"][0]["response_headers"]["www-authenticate"],
            "Bearer ***",
        )
        self.assertTrue(any("tools/list" in failure for failure in artifact["failures"]))
        self.assertTrue(any("tools/call unknown" in failure for failure in artifact["failures"]))

    def test_bearer_is_redacted_before_text_fallback_truncation(self):
        token = HostileBodyTests.TOKEN
        body = "🧪" * 5459 + token
        self.assertLessEqual(len(body.encode("utf-8")), proveout.MAX_BODY_BYTES)
        code, stdout, artifact, raw = HostileBodyTests().run_main({
            0: (200, {}, body),
        })
        self.assertEqual(code, 1)
        self.assertFalse(
            token[:28] in stdout, "partial bearer prefix escaped in stdout"
        )
        self.assertFalse(
            token[:28].encode() in raw, "partial bearer prefix escaped in receipt"
        )
        self.assertEqual(
            artifact["probes"][0]["body"]["_text"][-3:], "***"
        )

    def test_nonstandard_json_numbers_keep_the_receipt_parseable(self):
        def reject_constant(value):
            raise ValueError(f"non-standard JSON constant: {value}")

        for body in ("NaN", "Infinity", "-Infinity", "1e999"):
            with self.subTest(body=body):
                code, _stdout, artifact, raw = HostileBodyTests().run_main({
                    0: (200, {}, body),
                })
                self.assertEqual(code, 1)
                self.assertEqual(artifact["probes"][0]["body"], {"_text": body})
                json.loads(raw.decode("utf-8"), parse_constant=reject_constant)

    def test_unicode_and_control_text_fallback_serialization_is_bounded(self):
        cap = proveout.MAX_BODY_BYTES
        for character in ("�", "\x00", "🧪"):
            with self.subTest(character=character):
                code, stdout, artifact, raw = HostileBodyTests().run_main({
                    0: (200, {}, character * cap),
                })
                self.assertEqual(code, 1)
                rendered_body = json.dumps(
                    artifact["probes"][0]["body"],
                    separators=(",", ":"),
                ).encode("utf-8")
                self.assertLessEqual(
                    len(rendered_body), cap + len(b'{"_text":""}')
                )
                self.assertLess(len(stdout.encode("utf-8")), 2 * cap)
                self.assertLess(len(raw), 2 * cap)

    def test_json_null_remains_null_in_display(self):
        self.assertIsNone(proveout.parse_json_or_text("null"))

    def test_unicode_json_display_fallback_does_not_reject_valid_shape(self):
        padding = "🧪" * 15000
        cases = [
            (0, {"jsonrpc": "2.0", "id": 1, "result": {
                "protocolVersion": "2025-06-18", "pad": padding}}),
            (1, {"jsonrpc": "2.0", "id": 2, "result": {"tools": shipped_tools(), "pad": padding}}),
        ] + [(index, {"jsonrpc": "2.0", "id": index + 1, "result": {
            "content": [{"type": "text", "text": padding}],
            "structuredContent": {"available": True, "rows": [], "truncated": False},
        }}) for index in range(2, 2 + len(proveout.EXPECTED_TOOLS))] + [
            (7, {"jsonrpc": "2.0", "id": 8,
                 "error": {"code": -32601, "message": padding}}),
        ]
        for index, envelope in cases:
            with self.subTest(probe=HostileBodyTests.PROBES[index]):
                body = json.dumps(envelope, ensure_ascii=False, separators=(",", ":"))
                self.assertLessEqual(len(body.encode("utf-8")), proveout.MAX_BODY_BYTES)
                self.assertGreater(
                    len(json.dumps(envelope, separators=(",", ":"), sort_keys=True).encode("utf-8")),
                    proveout.MAX_BODY_BYTES,
                )

                code, stdout, artifact, raw = HostileBodyTests().run_main({
                    index: (200, {}, body),
                })

                self.assertEqual(code, 0)
                self.assertEqual(artifact["failures"], [])
                self.assertEqual(set(artifact["probes"][index]["body"]), {"_text"})
                rendered_body = json.dumps(
                    artifact["probes"][index]["body"], separators=(",", ":")
                ).encode("utf-8")
                self.assertLessEqual(
                    len(rendered_body), proveout.MAX_BODY_BYTES + len(b'{"_text":""}')
                )
                self.assertLess(len(stdout.encode("utf-8")), 2 * proveout.MAX_BODY_BYTES)
                self.assertLess(len(raw), 2 * proveout.MAX_BODY_BYTES)

    def test_reflected_bearer_in_nested_json_keys_is_redacted(self):
        token = HostileBodyTests.TOKEN
        code, stdout, artifact, raw = HostileBodyTests().run_main({
            0: (200, {}, json.dumps({token: {token: token}})),
        })

        self.assertEqual(code, 1)
        self.assertEqual(
            artifact["probes"][0]["body"], {"***": {"***": "***"}}
        )
        self.assertNotIn(token, stdout)
        self.assertNotIn(token.encode(), raw)


if __name__ == "__main__":
    unittest.main()
