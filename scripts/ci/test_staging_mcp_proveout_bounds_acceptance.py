"""Acceptance pins for bounded staging-MCP prove-out intake.

QA-owned acceptance suite for the build card `staging MCP prove-out: bound
response intake and artifact/log echo`. The Builder owns
`scripts/staging_mcp_proveout.py`; this file only reads it and never edits
builder-owned files (owner 2026-10-12 pairing rule). Corrections to this
file belong to the Test Author; the Builder must not edit it.

Pinned contract (offline, localhost only, no staging calls, no secrets in
fixtures):

- `staging_mcp_proveout.MAX_BODY_BYTES == 64 * 1024`, matching the
  strictest sibling reader (`staging_probe.py` 64 KiB,
  `prod_passive_probe.py` 64 KiB). The credentialed staging reader must be
  no looser than its siblings.
- `staging_mcp_proveout.BodyTooLarge` (an `Exception` subclass) is raised
  by `do_post` on BOTH read paths (normal and `HTTPError`) when the body
  exceeds the cap, having read at most `CAP + 1` bytes. The refusal lands
  before JSON parsing: an over-cap body that is also invalid or deeply
  nested JSON is still a `BodyTooLarge`, never a parse error. The message
  carries the fixed vocabulary `over the size cap`
  (`staging_probe.BodyTooLarge` wording) and never echoes body bytes or
  the bearer.
- `main()` maps the refusal to per-probe failures carrying the fixed
  vocabulary `over the body cap` (both siblings' failure wording), naming
  the probe — never shape-mismatch strings for over-cap input. In-cap
  malformed input keeps its ordinary shape verdicts. The bearer never
  appears in failures, stdout or the artifact.
- `parse_json_or_text` truncates the `_text` fallback to
  `body[:MAX_BODY_BYTES]` (identity for small bodies). The receipt schema
  stays parseable and stdout plus artifact stay bounded.

Seams: `do_post` cap tests drive a real `ThreadingHTTPServer` fixture
(real transport, no mocks), extending the pattern in
`scripts/ci/test_staging_mcp_proveout.py`. `main()` mapping tests patch
`do_post` because the `:139` URL/host fence (owned by a sibling card)
refuses plain-http localhost targets; the patched token/URL are dummy
values that are never sent anywhere.

Failing-first proof: on the pre-fix tree the cap-constant tests, the
refusal tests, the truncation test and the cap-failure mapping test fail
with assertion failures naming the missing behavior, while the control
tests (small/valid bodies, redirect file untouched) pass. The drip test
proves bounded intake without timing asserts: an unbounded `read()` on a
held-open body hits the client timeout and returns `network-error`, while
a capped read raises `BodyTooLarge` at `CAP + 1` bytes.
"""

import io
import json
import os
import sys
import tempfile
import threading
import time
import unittest
from contextlib import redirect_stdout
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

import staging_mcp_proveout as proveout
from scripts.ci.test_staging_mcp_proveout import shipped_tools

# 64 KiB, the strictest sibling cap (staging_probe, prod_passive_probe).
# The Builder implements proveout.MAX_BODY_BYTES at exactly this value;
# tests resolve the module constant (failing first) and use the literal
# only to size fixture bodies deterministically.
EXPECTED_CAP_BYTES = 64 * 1024

# Dummy bearer in the style of the existing prove-out tests. Long enough
# for main()'s minimum-length pre-flight check; never sent anywhere.
BEARER = "test-bearer-cap-probe-00000000000000"

PAYLOAD = {"jsonrpc": "2.0", "id": 1, "method": "tools/list"}


def require_cap():
    """Return (MAX_BODY_BYTES, BodyTooLarge) or fail with the missing piece."""
    cap = getattr(proveout, "MAX_BODY_BYTES", None)
    exc = getattr(proveout, "BodyTooLarge", None)
    return cap, exc


class CapConstantTests(unittest.TestCase):
    def test_cap_constant_is_64kib(self):
        cap, _ = require_cap()
        self.assertEqual(
            cap, EXPECTED_CAP_BYTES,
            "proveout.MAX_BODY_BYTES must be 64 KiB, matching staging_probe; "
            f"got {cap!r}",
        )

    def test_refusal_exception_exists(self):
        _, exc = require_cap()
        self.assertTrue(
            isinstance(exc, type) and issubclass(exc, Exception),
            "proveout.BodyTooLarge must be an Exception subclass; "
            f"got {exc!r}",
        )


class _FixedBody(BaseHTTPRequestHandler):
    status = 200
    body = b"{}"

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0") or "0")
        if length:
            self.rfile.read(length)
        body = type(self).body
        self.send_response(type(self).status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


class _DripBody(BaseHTTPRequestHandler):
    """Write CAP+1 bytes, then hold the connection open without EOF.

    Close-delimited (no Content-Length): an unbounded read() blocks until
    the client timeout, while a read capped at CAP+1 returns at once.
    """

    hold_seconds = 6.0

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0") or "0")
        if length:
            self.rfile.read(length)
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(b"d" * (EXPECTED_CAP_BYTES + 1))
        self.wfile.flush()
        time.sleep(type(self).hold_seconds)
        try:
            self.wfile.write(b"tail-never-read")
        except (BrokenPipeError, ConnectionResetError, ValueError):
            pass

    def log_message(self, *args):
        pass


def run_server(handler):
    server = ThreadingHTTPServer(("127.0.0.1", 0), handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server, thread


def stop_server(server, thread):
    server.shutdown()
    thread.join(timeout=5)
    server.server_close()


class DoPostBoundTests(unittest.TestCase):
    def _post_fixed(self, status, body, timeout=10):
        _FixedBody.status = status
        _FixedBody.body = body
        server, thread = run_server(_FixedBody)
        try:
            url = f"http://127.0.0.1:{server.server_port}/mcp"
            return proveout.do_post(url, PAYLOAD, BEARER, timeout=timeout)
        finally:
            stop_server(server, thread)

    def _assert_refused(self, thunk, what):
        cap, exc = require_cap()
        self.assertIsNotNone(
            cap, "proveout.MAX_BODY_BYTES is not defined; over-cap bodies "
            "must be refused"
        )
        self.assertTrue(
            isinstance(exc, type) and issubclass(exc, Exception),
            "proveout.BodyTooLarge is not defined; over-cap bodies must be "
            "refused",
        )
        try:
            result = thunk()
        except Exception as raised:
            # Narrowed by the assertIsInstance below; never swallowed.
            self.assertIsInstance(
                raised, exc,
                f"{what}: expected BodyTooLarge, got "
                f"{type(raised).__name__}: {raised}",
            )
            message = str(raised)
            self.assertIn(
                "over the size cap", message,
                f"{what}: refusal must use the fixed cap vocabulary; got "
                f"{message!r}",
            )
            self.assertNotIn(
                BEARER, message,
                f"{what}: refusal must never carry the bearer",
            )
            return message
        status = result[0] if isinstance(result, tuple) else result
        self.fail(
            f"{what}: over-cap body was accepted (status {status}) "
            "instead of refused"
        )

    def test_normal_path_over_cap_refused(self):
        sentinel = b"normal-path-sentinel-body-bytes"
        body = b'{"ok":true,"pad":"' + sentinel + b'"' \
            + b" " * (EXPECTED_CAP_BYTES + 128) + b"}"
        message = self._assert_refused(
            lambda: self._post_fixed(200, body), "normal path"
        )
        self.assertNotIn(
            "normal-path-sentinel-body-bytes", message,
            "refusal must never echo body bytes",
        )

    def test_error_path_over_cap_refused(self):
        body = b'{"error":{"code":-32000}}' + b" " * (EXPECTED_CAP_BYTES + 64)
        message = self._assert_refused(
            lambda: self._post_fixed(500, body), "HTTPError path"
        )
        self.assertNotIn(BEARER, message)

    def test_boundary_cap_plus_one_refused(self):
        self._assert_refused(
            lambda: self._post_fixed(200, b"f" * (EXPECTED_CAP_BYTES + 1)),
            "CAP+1 boundary",
        )

    def test_over_cap_invalid_json_refused_before_parse(self):
        # A parse-first implementation would return this as _text instead
        # of raising; the cap verdict must win over JSON parsing.
        body = b"not-json{{{garbage" + b"g" * (EXPECTED_CAP_BYTES + 64)
        self._assert_refused(
            lambda: self._post_fixed(200, body), "over-cap invalid JSON"
        )

    def test_over_cap_deeply_nested_refused_before_parse(self):
        # A parse-first implementation would blow the decoder recursion
        # limit here; the cap verdict must land before any JSON decode.
        depth = 35_000
        body = b"[" * depth + b"]" * depth
        self.assertGreater(len(body), EXPECTED_CAP_BYTES)
        self._assert_refused(
            lambda: self._post_fixed(200, body), "over-cap nested JSON"
        )

    def test_held_open_body_refused_without_reading_to_eof(self):
        # The server holds the body open after CAP+1 bytes: an unbounded
        # read() would hit the client timeout and return network-error,
        # while a capped read raises at once. No timing assert needed;
        # raise-vs-return is the proof.
        server, thread = run_server(_DripBody)
        try:
            url = f"http://127.0.0.1:{server.server_port}/mcp"
            self._assert_refused(
                lambda: proveout.do_post(url, PAYLOAD, BEARER, timeout=3),
                "held-open body",
            )
        finally:
            stop_server(server, thread)

    def test_exact_cap_valid_input_passes(self):
        # Boundary guard: exact-cap input must not be refused (no
        # off-by-one `>=`), on either read path.
        body = b"e" * EXPECTED_CAP_BYTES
        status, _, text = self._post_fixed(200, body)
        self.assertEqual(status, 200)
        self.assertEqual(len(text.encode("utf-8")), EXPECTED_CAP_BYTES)
        status, _, text = self._post_fixed(500, body)
        self.assertEqual(status, 500)
        self.assertEqual(len(text.encode("utf-8")), EXPECTED_CAP_BYTES)

    def test_small_valid_body_unchanged(self):
        status, _, text = self._post_fixed(200, b'{"result":{"ok":true}}')
        self.assertEqual(status, 200)
        self.assertEqual(json.loads(text), {"result": {"ok": True}})


class TextFallbackBoundTests(unittest.TestCase):
    def test_text_fallback_truncated_to_cap(self):
        cap, _ = require_cap()
        self.assertIsNotNone(
            cap, "proveout.MAX_BODY_BYTES is not defined; the _text "
            "fallback must truncate to it"
        )
        body = "not json {{{ garbage " + "t" * (2 * cap + 100)
        self.assertEqual(
            proveout.parse_json_or_text(body), {"_text": body[:cap]},
            "the _text fallback must keep at most the first CAP bytes",
        )

    def test_small_text_fallback_unchanged(self):
        self.assertEqual(
            proveout.parse_json_or_text("plain failure text"),
            {"_text": "plain failure text"},
        )

    def test_valid_json_unchanged(self):
        self.assertEqual(proveout.parse_json_or_text('{"a":1}'), {"a": 1})

    def test_deeply_nested_in_cap_body_does_not_raise(self):
        # Depth 15000 trips the decoder recursion limit (observed trip
        # between 5000 and 10000 on CPython 3.13) at 30000 bytes, under
        # the 64 KiB cap.
        body = "[" * 15000 + "]" * 15000
        self.assertLess(len(body), EXPECTED_CAP_BYTES)
        self.assertEqual(
            proveout.parse_json_or_text(body), {"_text": body},
            "in-cap nested JSON must degrade to _text, never raise",
        )


class _RaiseCap:
    """Script sentinel: this do_post call raises BodyTooLarge."""


def valid_envelope(kind, rpc_id):
    """A main()-satisfying response for probe `kind` (offline fixture)."""
    headers = {"content-type": "application/json"}
    if kind == "initialize":
        return 200, headers, json.dumps({
            "jsonrpc": "2.0", "id": rpc_id,
            "result": {"protocolVersion": "2025-06-18"},
        })
    if kind == "tools/list":
        return 200, headers, json.dumps({
            "jsonrpc": "2.0", "id": rpc_id,
            "result": {"tools": shipped_tools()},
        })
    if kind.startswith("tools/call "):
        return 200, headers, json.dumps({
            "jsonrpc": "2.0", "id": rpc_id,
            "result": {
                "content": [{"type": "text", "text": "ok"}],
                "structuredContent": (
                    {"available": True, "rows": [], "truncated": False}
                    if kind == "tools/call get_audit"
                    else {"implemented": False}
                ),
            },
        })
    if kind == "unknown":
        return 200, headers, json.dumps({
            "jsonrpc": "2.0", "id": rpc_id,
            "error": {"code": -32601, "message": "unknown tool"},
        })
    if kind in ("wrong bearer", "no bearer"):
        return 401, {"www-authenticate": "Bearer"}, json.dumps({
            "jsonrpc": "2.0", "id": rpc_id,
            "error": {"code": -32001, "message": "unauthorized"},
        })
    raise AssertionError(f"unknown probe kind {kind}")


PROBE_SCRIPT = (
    ["initialize", "tools/list"]
    + [f"tools/call {t}" for t in proveout.EXPECTED_TOOLS]
    + ["unknown", "wrong bearer", "no bearer"]
)


def valid_script():
    return [valid_envelope(kind, rpc_id + 1)
            for rpc_id, kind in enumerate(PROBE_SCRIPT)]


class MainCapMappingTests(unittest.TestCase):
    """main() mapping via patched do_post (the URL fence owns real URLs)."""

    def _run_main(self, script):
        calls = []

        def fake_do_post(url, payload, token_or_none, timeout=25):
            calls.append((url, payload, token_or_none))
            item = script.pop(0)
            if item is _RaiseCap:
                raise proveout.BodyTooLarge("response body over the size cap")
            return item

        with tempfile.TemporaryDirectory() as tmp:
            output = str(Path(tmp) / "receipt.json")
            argv = ["staging_mcp_proveout.py", "--output", output]
            env = {
                proveout.TOKEN_ENV: BEARER,
                # Fence-passing dummy URL (do_post is mocked; never sent).
                proveout.URL_ENV: proveout.DEFAULT_URL,
            }
            with mock.patch.object(proveout, "do_post", fake_do_post), \
                    mock.patch.object(sys, "argv", argv), \
                    mock.patch.dict(os.environ, env, clear=False):
                buf = io.StringIO()
                with redirect_stdout(buf):
                    try:
                        code = proveout.main()
                    except Exception as raised:
                        self.fail(
                            "main() raised "
                            f"{type(raised).__name__} ({raised}); over-cap "
                            "input must map to a probe failure, never "
                            "propagate"
                        )
            stdout = buf.getvalue()
            raw = Path(output).read_bytes()
            artifact = json.loads(raw.decode("utf-8"))
        self.assertEqual(script, [], "main() must run all ten probes")
        return code, stdout, artifact, raw, calls

    def _assert_bearer_free(self, stdout, artifact, raw, failures):
        joined = "\n".join(failures)
        self.assertNotIn(BEARER, joined, "failures must never carry the bearer")
        self.assertNotIn(BEARER, stdout, "stdout must never carry the bearer")
        self.assertNotIn(
            BEARER, raw.decode("utf-8"),
            "the artifact must never carry the bearer",
        )
        self.assertNotIn(BEARER, json.dumps(artifact))

    def test_over_cap_probe_becomes_cap_failure(self):
        script = valid_script()
        script[0] = _RaiseCap
        code, stdout, artifact, raw, _ = self._run_main(script)
        self.assertEqual(code, 1)
        failures = artifact["failures"]
        self.assertTrue(
            any("over the body cap" in f and "initialize" in f
                for f in failures),
            f"over-cap input must fail the probe with the fixed cap "
            f"verdict; got {failures!r}",
        )
        joined = "\n".join(failures)
        self.assertNotIn(
            "protocolVersion", joined,
            "an over-cap probe must not report a shape mismatch",
        )
        self.assertNotIn(
            "expected exactly", joined,
            "an over-cap probe must not report a shape mismatch",
        )
        self._assert_bearer_free(stdout, artifact, raw, failures)
        self.assertEqual(artifact["schema_version"], proveout.SCHEMA_VERSION)
        self.assertLess(
            len(stdout.encode("utf-8")), EXPECTED_CAP_BYTES,
            "stdout must stay bounded when a body is refused",
        )
        self.assertLess(
            len(raw), EXPECTED_CAP_BYTES,
            "the artifact must stay bounded when a body is refused",
        )

    def test_exact_cap_valid_body_passes_end_to_end(self):
        # Boundary guard: an exact-cap valid envelope must flow through
        # untouched (no off-by-one refusal, no truncation).
        base = {"jsonrpc": "2.0", "id": 1,
                "result": {"protocolVersion": "2025-06-18", "pad": ""}}
        text = json.dumps(base)
        pad_len = EXPECTED_CAP_BYTES - len(text.encode("utf-8"))
        self.assertGreater(pad_len, 0)
        base["result"]["pad"] = "p" * pad_len
        exact = json.dumps(base)
        self.assertEqual(len(exact.encode("utf-8")), EXPECTED_CAP_BYTES)
        script = valid_script()
        script[0] = (200, {"content-type": "application/json"}, exact)
        code, _, artifact, _, _ = self._run_main(script)
        self.assertEqual(artifact["failures"], [])
        self.assertEqual(code, 0)
        kept = artifact["probes"][0]["body"]["result"]["pad"]
        self.assertEqual(len(kept), pad_len, "exact-cap bodies keep full bytes")

    def test_valid_script_passes_end_to_end(self):
        code, stdout, artifact, raw, calls = self._run_main(valid_script())
        self.assertEqual(artifact["failures"], [])
        self.assertEqual(code, 0)
        self.assertEqual(len(calls), 10)
        self._assert_bearer_free(stdout, artifact, raw, [])

    def test_in_cap_malformed_keeps_shape_verdict(self):
        # Control: the cap path must not swallow ordinary validation.
        script = valid_script()
        script[0] = (200, {"content-type": "application/json"},
                     '{"result": {}}')
        code, _, artifact, _, _ = self._run_main(script)
        self.assertEqual(code, 1)
        joined = "\n".join(artifact["failures"])
        self.assertIn("protocolVersion", joined)
        self.assertNotIn("body cap", joined)

    def test_nested_in_cap_body_does_not_crash_main(self):
        # Same decoder-tripping depth as the parse-level test above.
        body = "[" * 15000 + "]" * 15000
        self.assertLess(len(body), EXPECTED_CAP_BYTES)
        script = valid_script()
        script[0] = (200, {"content-type": "application/json"}, body)
        code, stdout, artifact, raw, _ = self._run_main(script)
        self.assertEqual(code, 1)
        joined = "\n".join(artifact["failures"])
        self.assertIn("protocolVersion", joined)
        self.assertNotIn("body cap", joined)
        self.assertLess(len(stdout.encode("utf-8")), EXPECTED_CAP_BYTES)
        self.assertLess(len(raw), EXPECTED_CAP_BYTES)


if __name__ == "__main__":
    unittest.main()
