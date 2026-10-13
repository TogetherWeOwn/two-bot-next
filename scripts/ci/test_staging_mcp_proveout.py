"""Offline prove-out redirect guard; never touches staging.

do_post must not follow 301/302/303 redirects: the Authorization bearer
must stay on STAGING_MCP_URL and never reach a Location target.
"""

import sys
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

import staging_mcp_proveout as proveout  # noqa: E402


class _Target(BaseHTTPRequestHandler):
    hits = []

    def do_GET(self):  # noqa: N802
        _Target.hits.append(
            (self.headers.get("Authorization"), self.path)
        )
        body = b"ok"
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):  # noqa: D102
        pass


class _Redirector(BaseHTTPRequestHandler):
    target_port = 0
    code = 302

    def do_POST(self):  # noqa: N802
        length = int(self.headers.get("Content-Length", "0") or "0")
        if length:
            self.rfile.read(length)
        self.send_response(type(self).code)
        self.send_header(
            "Location", f"http://127.0.0.1:{type(self).target_port}/elsewhere"
        )
        self.end_headers()

    def log_message(self, *args):  # noqa: D102
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


if __name__ == "__main__":
    unittest.main()
