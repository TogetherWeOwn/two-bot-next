"""Offline prove-out guards; never touches staging.

do_post must not follow 301/302/303 redirects: the Authorization bearer
must stay on STAGING_MCP_URL and never reach a Location target.
The URL fence allows only the exact staging https /mcp URL, and every
response body read is capped at MCP_MAX_BODY_BYTES.
"""

import io
import json
import os
import sys
import tempfile
import threading
import unittest
import urllib.error
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

import staging_mcp_proveout as proveout


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
                envelope["result"] = {"tools": [{"name": t} for t in proveout.EXPECTED_TOOLS]}
            else:
                tool = payload["params"]["name"]
                if tool == "get_audit":
                    envelope.update(audit_reply)
                elif tool == "no_such_tool":
                    envelope["error"] = {"code": -32601}
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
        self.assertEqual(self.run_proveout({"error": {
            "code": -32000, "message": "container request failed",
        }}), (0, []))

    def test_other_audit_errors_still_fail(self):
        code, failures = self.run_proveout({"error": {"code": -32603}})
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
        self.assertEqual(proveout.MCP_MAX_BODY_BYTES, 64 * 1024)

    def test_exact_cap_valid_input_passes(self):
        seen = []
        body = b'{"result":{}}' + b" " * (proveout.MCP_MAX_BODY_BYTES - len(b'{"result":{}}'))
        self.assertEqual(len(body), proveout.MCP_MAX_BODY_BYTES)
        opener = mock.Mock()
        opener.open.return_value = self.response(body, seen)
        with mock.patch("urllib.request.build_opener", return_value=opener):
            status, _headers, text = proveout.do_post(self.URL, {"id": 1}, self.TOKEN)
        self.assertEqual(status, 200)
        self.assertEqual(text, body.decode("utf-8"))
        self.assertEqual(seen, [proveout.MCP_MAX_BODY_BYTES + 1])

    def test_oversize_body_refuses_with_fixed_message(self):
        sentinel = "SENTINEL_BODY_mcp_proveout_padded"
        base = f'{{"result":{{"note":"{sentinel}"}}}}'.encode()
        body = base + b" " * (proveout.MCP_MAX_BODY_BYTES + 1 - len(base))
        self.assertEqual(len(body), proveout.MCP_MAX_BODY_BYTES + 1)
        seen = []
        opener = mock.Mock()
        opener.open.return_value = self.response(body, seen)
        with mock.patch("urllib.request.build_opener", return_value=opener):
            status, _headers, text = proveout.do_post(self.URL, {"id": 1}, self.TOKEN)
        self.assertEqual(status, 200)
        self.assertEqual(text, proveout.BODY_TOO_LARGE_TEXT)
        self.assertNotIn(sentinel, text)
        self.assertNotIn(self.TOKEN, text)
        self.assertEqual(seen, [proveout.MCP_MAX_BODY_BYTES + 1])

    def test_oversize_http_error_body_refuses_the_same_way(self):
        sentinel = "SENTINEL_BODY_mcp_proveout_error_padded"
        body = sentinel.encode() + b"y" * (proveout.MCP_MAX_BODY_BYTES + 1 - len(sentinel))
        self.assertEqual(len(body), proveout.MCP_MAX_BODY_BYTES + 1)
        seen = []
        opener = mock.Mock()
        opener.open.side_effect = self.http_error(body, seen)
        with mock.patch("urllib.request.build_opener", return_value=opener):
            status, _headers, text = proveout.do_post(self.URL, {"id": 1}, self.TOKEN)
        self.assertEqual(status, 500)
        self.assertEqual(text, proveout.BODY_TOO_LARGE_TEXT)
        self.assertNotIn(sentinel, text)
        self.assertNotIn(self.TOKEN, text)
        self.assertEqual(seen, [proveout.MCP_MAX_BODY_BYTES + 1])


if __name__ == "__main__":
    unittest.main()
