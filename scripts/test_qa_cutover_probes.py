"""Cutover acceptance probe regressions: fixture doubles only, no network or databases."""

import http.server
import io
import json
import sys
import threading
import time
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import qa_cutover_probes as probe  # noqa: E402

HEALTH = (200, json.dumps({"status": "ok"}).encode())
PARKED = (503, json.dumps({
    "components": [["process", "ready"], ["gateway", "down"],
                   ["database", "down"], ["token_invalid", "ready"]],
    "jobs": {"counter": {"parked": True, "running": False}},
    "build_revision": "0123456789abcdef0123456789abcdef01234567", "build_id": "700-1",
}).encode())
READY = (200, json.dumps({
    "components": [["process", "ready"], ["gateway", "ready"]],
    "jobs": {"counter": {"parked": False, "running": True}},
    "build_revision": "0123456789abcdef0123456789abcdef01234567", "build_id": "700-1",
}).encode())
FENCED = (503, json.dumps({"error": "ownership_fenced", "reason": "parked"}).encode())


def double(mapping):
    def fetch(url):
        for suffix, answer in mapping.items():
            if url.endswith(suffix):
                if isinstance(answer, Exception):
                    raise answer
                return answer
        raise AssertionError(f"unexpected GET {url}")
    return fetch


def run(fetch_fn, *argv):
    out = io.StringIO()
    with redirect_stdout(out):
        code = probe.main(list(argv), fetch_fn=fetch_fn)
    return code, out.getvalue()


class UserAgentTest(unittest.TestCase):
    """The default urllib agent is refused by the edge (Cloudflare 1010), so a
    real fetch must name itself. Loopback server only; no external network."""

    def test_fetch_sends_a_named_agent(self):
        seen = []

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                seen.append(self.headers.get("User-Agent"))
                self.send_response(200)
                self.end_headers()
                self.wfile.write(b"{}")

            def log_message(self, *args):
                pass

        server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            status, _ = probe.fetch(f"http://127.0.0.1:{server.server_port}/health", 5)
        finally:
            server.shutdown()
            server.server_close()
            thread.join(5)
        self.assertEqual(status, 200)
        self.assertEqual(seen, [probe.USER_AGENT])
        self.assertNotIn("Python-urllib", probe.USER_AGENT)


class BaseUrlTest(unittest.TestCase):
    def test_rejects_credentials_and_paths(self):
        for bad in ("http://user:pw@h/", "http://h/path", "ftp://h/"):
            args = probe.parse_args(["--base-url", bad])
            _, results = probe.run(args, fetch_fn=double({}))
            self.assertFalse(results[0].ok)
            self.assertIn("refusing", results[0].reason)

    def test_accepts_bare_origin_with_port(self):
        base = probe.normalize_base("http://127.0.0.1:8080/")
        self.assertEqual(base, "http://127.0.0.1:8080")

    def test_ipv6_brackets_survive(self):
        base = probe.normalize_base("http://[::1]:8080/")
        self.assertEqual(base, "http://[::1]:8080")

    def test_unusable_host_refused_before_any_fetch(self):
        # An overlong DNS label would raise UnicodeError inside getaddrinfo
        # (a ValueError, not an OSError), escaping the fetch guards with a
        # traceback; refuse it at admission with fixed vocabulary instead.
        args = probe.parse_args(["--base-url", f"http://{'a' * 70}/"])
        _, results = probe.run(args, fetch_fn=double({}))
        self.assertFalse(results[0].ok)
        self.assertIn("refusing", results[0].reason)
        self.assertNotIn("a" * 16, results[0].reason)
        self.assertEqual(results[0].code, "origin_bad_host")

    def test_usable_edge_hosts_still_admitted(self):
        for raw, host in ((f"http://{'a' * 63}/", 'a' * 63),
                          ("http://under_score.example/", "under_score.example"),
                          ("http://example./", "example."),
                          ("http://PROBE.EXAMPLE/", "probe.example")):
            with self.subTest(raw=raw):
                self.assertEqual(probe.normalize_base(raw), f"http://{host}")

    def test_transport_unicode_error_is_a_fixed_liveness_fail(self):
        def failing_fetch(url):
            raise UnicodeEncodeError("idna", "x" * 70, 0, 70, "label too long")

        args = probe.parse_args(["--base-url", "http://h/"])
        _, results = probe.run(args, fetch_fn=failing_fetch)
        self.assertEqual(len(results), 1)
        self.assertFalse(results[0].ok)
        self.assertIn("did not respond", results[0].reason)
        self.assertNotIn("Traceback", results[0].reason)
        self.assertEqual(results[0].code, "liveness_no_response")

    def test_truthful_503_with_gateway_ready_stays_green(self):
        body = (503, json.dumps({
            "components": [["process", "ready"], ["gateway", "ready"],
                           ["database", "down"]],
            "jobs": {"counter": {"parked": True, "running": False}},
            "build_revision": "0123456789abcdef0123456789abcdef01234567", "build_id": "700-1",
        }).encode())
        code, out = run(double({"/health": HEALTH, "/readyz": body}),
                        "--base-url", "http://h/")
        self.assertEqual(code, 0, out)
        self.assertIn("PASS gateway-state:", out)

    def test_truthful_503_with_gateway_ready_fails_expect_ready(self):
        body = (503, json.dumps({
            "components": [["process", "ready"], ["gateway", "ready"],
                           ["database", "down"]],
            "jobs": {"counter": {"parked": True, "running": False}},
            "build_revision": "0123456789abcdef0123456789abcdef01234567", "build_id": "700-1",
        }).encode())
        code, out = run(double({"/health": HEALTH, "/readyz": body}),
                        "--base-url", "http://h/", "--expect-ready")
        self.assertEqual(code, 1)
        self.assertIn("FAIL gateway-state:", out)
        self.assertIn("service not ready", out)


class ParkedPreviewTest(unittest.TestCase):
    def test_parked_preview_stays_green(self):
        code, out = run(double({"/health": HEALTH, "/readyz": PARKED}),
                        "--base-url", "http://127.0.0.1:8080")
        self.assertEqual(code, 0, out)
        for name in ("liveness", "readiness-shape", "gateway-state",
                     "jobs-map", "fence-watch"):
            self.assertIn(f"PASS {name}:", out)
        self.assertIn("5/5 checks passed", out)

    def test_expect_ready_fails_parked(self):
        code, out = run(double({"/health": HEALTH, "/readyz": PARKED}),
                        "--base-url", "http://127.0.0.1:8080", "--expect-ready")
        self.assertEqual(code, 1)
        self.assertIn("FAIL gateway-state:", out)
        self.assertIn("--expect-ready", out)

    def test_ready_service_passes_expect_ready(self):
        code, out = run(double({"/health": HEALTH, "/readyz": READY}),
                        "--base-url", "http://h/", "--expect-ready")
        self.assertEqual(code, 0, out)
        self.assertIn("gateway ready", out)


class LyingGateTest(unittest.TestCase):
    def test_200_with_parked_gateway_fails(self):
        body = (200, json.dumps({
            "components": [["process", "ready"], ["gateway", "down"]],
            "jobs": {"counter": {"parked": True, "running": False}},
            "build_revision": "0123456789abcdef0123456789abcdef01234567", "build_id": "700-1",
        }).encode())
        code, out = run(double({"/health": HEALTH, "/readyz": body}),
                        "--base-url", "http://h/")
        self.assertEqual(code, 1)
        self.assertIn("FAIL readiness-shape:", out)
        self.assertIn("disagrees with its breakdown", out)

    def test_503_with_ready_gateway_fails(self):
        body = (503, json.dumps({
            "components": [["process", "ready"], ["gateway", "ready"]],
            "jobs": {"counter": {"parked": True, "running": False}},
            "build_revision": "0123456789abcdef0123456789abcdef01234567", "build_id": "700-1",
        }).encode())
        code, out = run(double({"/health": HEALTH, "/readyz": body}),
                        "--base-url", "http://h/")
        self.assertEqual(code, 1)
        self.assertIn("FAIL readiness-shape:", out)

    def test_unknown_status_vocabulary_fails(self):
        body = (503, json.dumps({
            "components": [["process", "ready"], ["gateway", "bogus"]],
            "jobs": {"counter": {"parked": True, "running": False}},
            "build_revision": "0123456789abcdef0123456789abcdef01234567", "build_id": "700-1",
        }).encode())
        code, out = run(double({"/health": HEALTH, "/readyz": body}),
                        "--base-url", "http://h/")
        self.assertEqual(code, 1)
        self.assertIn("FAIL readiness-shape:", out)

    def test_missing_build_identity_fails(self):
        body = (503, json.dumps({
            "components": [["process", "ready"], ["gateway", "down"]],
            "jobs": {"counter": {"parked": True, "running": False}},
        }).encode())
        code, out = run(double({"/health": HEALTH, "/readyz": body}),
                        "--base-url", "http://h/")
        self.assertEqual(code, 1)
        self.assertIn("build_revision", out)


class FenceTest(unittest.TestCase):
    def test_ownership_refusal_fails(self):
        code, out = run(double({"/health": HEALTH, "/readyz": FENCED}),
                        "--base-url", "http://h/")
        self.assertEqual(code, 1)
        self.assertIn("FAIL readiness-shape:", out)
        self.assertIn("FAIL fence-watch:", out)

    def test_redirect_status_fails(self):
        body = (302, b"<html>redirect</html>")
        code, out = run(double({"/health": HEALTH, "/readyz": body}),
                        "--base-url", "http://h/")
        self.assertEqual(code, 1)
        self.assertIn("FAIL readiness-shape:", out)

    def test_health_mismatch_fails(self):
        code, out = run(double({"/health": (200, b'{"status":"bad"}'),
                                "/readyz": PARKED}),
                        "--base-url", "http://h/")
        self.assertEqual(code, 1)
        self.assertIn("FAIL liveness:", out)

    def test_jobs_map_malformed_fails(self):
        body = (503, json.dumps({
            "components": [["process", "ready"], ["gateway", "down"]],
            "jobs": {"counter": {"parked": "yes"}},
            "build_revision": "0123456789abcdef0123456789abcdef01234567", "build_id": "700-1",
        }).encode())
        code, out = run(double({"/health": HEALTH, "/readyz": body}),
                        "--base-url", "http://h/")
        self.assertEqual(code, 1)
        self.assertIn("FAIL jobs-map:", out)


class DuplicateComponentTest(unittest.TestCase):
    def dup_body(self, pairs, status):
        return (status, json.dumps({
            "components": [[name, value] for name, value in pairs],
            "jobs": {"counter": {"parked": True, "running": False}},
            "build_revision": "0123456789abcdef0123456789abcdef01234567", "build_id": "700-1",
        }).encode())

    def assert_rejected(self, pairs, status):
        _, raw = self.dup_body(pairs, status)
        with self.assertRaisesRegex(probe.ProbeError, "repeats a component name"):
            probe.check_readyz_shape(status, raw)

    def test_down_then_ready_rejected(self):
        self.assert_rejected([("process", "ready"), ("gateway", "down"),
                              ("gateway", "ready")], 200)

    def test_ready_then_down_rejected(self):
        self.assert_rejected([("process", "ready"), ("gateway", "ready"),
                              ("gateway", "down")], 503)

    def test_same_value_repeat_rejected(self):
        self.assert_rejected([("process", "ready"), ("gateway", "ready"),
                              ("process", "ready")], 200)

    def test_distinct_components_still_parse(self):
        _, raw = self.dup_body([("process", "ready"), ("gateway", "ready")], 200)
        state, _ = probe.check_readyz_shape(200, raw)
        self.assertEqual(state["gateway"], "ready")

    def test_diagnostic_echoes_no_component_data(self):
        sentinel = "SENTINEL_dup_cutover_abc123"
        _, raw = self.dup_body([("process", "ready"), ("gateway", "ready"),
                                (sentinel, "ready"), (sentinel, "down")], 503)
        with self.assertRaises(probe.ProbeError) as ctx:
            probe.check_readyz_shape(503, raw)
        self.assertIn("repeats a component name", str(ctx.exception))
        self.assertNotIn(sentinel, str(ctx.exception))

    def test_duplicate_fails_readiness_shape_end_to_end(self):
        body = self.dup_body([("process", "ready"), ("gateway", "down"),
                              ("gateway", "ready")], 200)
        code, out = run(double({"/health": HEALTH, "/readyz": body}),
                        "--base-url", "http://h/")
        self.assertEqual(code, 1)
        self.assertIn("FAIL readiness-shape:", out)
        self.assertIn("repeats a component name", out)


class DuplicateJsonKeysTest(unittest.TestCase):
    """Wire-object duplicate JSON keys refuse before any grading.

    Plain json.loads keeps the last duplicate, so a repeated top-level
    ``components`` can mask gateway=down as ready. Every object at any
    depth is strict: identical repeats and both conflicting orders fail
    closed with fixed vocabulary, never echoing keys or values.
    """

    READY_PAIR = '[["process", "ready"], ["gateway", "ready"]]'
    DOWN_PAIR = '[["process", "ready"], ["gateway", "down"]]'

    def test_top_level_components_down_then_ready_refused(self):
        raw = ('{"components": ' + self.DOWN_PAIR
               + ', "jobs": {"counter": {"parked": true, "running": false}}, '
               + '"build_revision": "0123456789abcdef0123456789abcdef01234567", "build_id": "700-1", '
               + '"components": ' + self.READY_PAIR + '}').encode()
        with self.assertRaises(probe.ProbeError) as ctx:
            probe.check_readyz_shape(200, raw)
        self.assertEqual(str(ctx.exception),
                         "/readyz 200 carries duplicate JSON object keys")

    def test_top_level_components_ready_then_down_refused(self):
        raw = ('{"components": ' + self.READY_PAIR
               + ', "jobs": {"counter": {"parked": true, "running": false}}, '
               + '"build_revision": "0123456789abcdef0123456789abcdef01234567", "build_id": "700-1", '
               + '"components": ' + self.DOWN_PAIR + '}').encode()
        with self.assertRaises(probe.ProbeError) as ctx:
            probe.check_readyz_shape(503, raw)
        self.assertEqual(str(ctx.exception),
                         "/readyz 503 carries duplicate JSON object keys")

    def test_top_level_components_identical_repeat_refused(self):
        raw = ('{"components": ' + self.READY_PAIR
               + ', "jobs": {"counter": {"parked": true, "running": false}}, '
               + '"build_revision": "0123456789abcdef0123456789abcdef01234567", "build_id": "700-1", '
               + '"components": ' + self.READY_PAIR + '}').encode()
        with self.assertRaises(probe.ProbeError) as ctx:
            probe.check_readyz_shape(200, raw)
        self.assertEqual(str(ctx.exception),
                         "/readyz 200 carries duplicate JSON object keys")

    def test_top_level_components_duplicate_fails_closed_end_to_end(self):
        raw = ('{"components": ' + self.DOWN_PAIR
               + ', "jobs": {"counter": {"parked": true, "running": false}}, '
               + '"build_revision": "0123456789abcdef0123456789abcdef01234567", "build_id": "700-1", '
               + '"components": ' + self.READY_PAIR + '}').encode()
        code, out = run(double({"/health": HEALTH, "/readyz": (200, raw)}),
                        "--base-url", "http://h/")
        self.assertEqual(code, 1, out)
        self.assertIn("FAIL readiness-shape:", out)
        self.assertIn("/readyz 200 carries duplicate JSON object keys", out)
        self.assertIn("FAIL gateway-state:", out)
        self.assertIn("FAIL jobs-map:", out)
        self.assertNotIn("gateway=ready", out)

    def test_health_status_identical_repeat_refused(self):
        raw = b'{"status": "ok", "status": "ok"}'
        with self.assertRaises(probe.ProbeError) as ctx:
            probe.check_liveness(200, raw)
        self.assertEqual(str(ctx.exception),
                         "/health carries duplicate JSON object keys")

    def test_health_status_conflicting_orders_refused(self):
        for raw in (b'{"status": "bad", "status": "ok"}',
                    b'{"status": "ok", "status": "bad"}'):
            with self.subTest(raw=raw):
                with self.assertRaises(probe.ProbeError) as ctx:
                    probe.check_liveness(200, raw)
                self.assertEqual(str(ctx.exception),
                                 "/health carries duplicate JSON object keys")

    def test_health_duplicate_fails_liveness_end_to_end(self):
        raw = b'{"status": "bad", "status": "ok"}'
        code, out = run(double({"/health": (200, raw), "/readyz": PARKED}),
                        "--base-url", "http://h/")
        self.assertEqual(code, 1, out)
        self.assertIn("FAIL liveness:", out)
        self.assertIn("/health carries duplicate JSON object keys", out)
        self.assertNotIn("bad", out)

    def test_health_unique_still_passes(self):
        code, out = run(double({"/health": HEALTH, "/readyz": PARKED}),
                        "--base-url", "http://h/")
        self.assertEqual(code, 0, out)
        self.assertIn("PASS liveness:", out)

    def test_nested_job_name_duplicate_both_orders_refused(self):
        fresh = '{"parked": false, "running": true}'
        parked = '{"parked": true, "running": false}'
        for jobs in (f'{{"counter": {fresh}, "counter": {parked}}}',
                     f'{{"counter": {parked}, "counter": {fresh}}}'):
            raw = ('{"components": ' + self.READY_PAIR + ', "jobs": ' + jobs
                   + ', "build_revision": "0123456789abcdef0123456789abcdef01234567", "build_id": "700-1"}').encode()
            with self.subTest(jobs=jobs):
                with self.assertRaises(probe.ProbeError) as ctx:
                    probe.check_readyz_shape(200, raw)
                self.assertEqual(str(ctx.exception),
                                 "/readyz 200 carries duplicate JSON object keys")

    def test_nested_job_name_identical_repeat_refused(self):
        jobs = '{"counter": {"parked": true, "running": false}, "counter": {"parked": true, "running": false}}'
        raw = ('{"components": ' + self.READY_PAIR + ', "jobs": ' + jobs
               + ', "build_revision": "0123456789abcdef0123456789abcdef01234567", "build_id": "700-1"}').encode()
        with self.assertRaises(probe.ProbeError) as ctx:
            probe.check_readyz_shape(200, raw)
        self.assertEqual(str(ctx.exception),
                         "/readyz 200 carries duplicate JSON object keys")

    def test_nested_job_field_duplicate_both_orders_refused(self):
        for entry in ('{"parked": false, "running": true, "parked": true}',
                      '{"parked": true, "running": true, "parked": false}',
                      '{"parked": false, "parked": false, "running": true}'):
            raw = ('{"components": ' + self.READY_PAIR
                   + ', "jobs": {"counter": ' + entry + '}, '
                   + '"build_revision": "0123456789abcdef0123456789abcdef01234567", "build_id": "700-1"}').encode()
            with self.subTest(entry=entry):
                with self.assertRaises(probe.ProbeError) as ctx:
                    probe.check_readyz_shape(200, raw)
                self.assertEqual(str(ctx.exception),
                                 "/readyz 200 carries duplicate JSON object keys")

    def test_nested_job_duplicate_fails_readiness_before_job_grading(self):
        jobs = '{"counter": {"parked": false, "running": true}, "counter": {"parked": true, "running": false}}'
        raw = ('{"components": ' + self.READY_PAIR + ', "jobs": ' + jobs
               + ', "build_revision": "0123456789abcdef0123456789abcdef01234567", "build_id": "700-1"}').encode()
        code, out = run(double({"/health": HEALTH, "/readyz": (200, raw)}),
                        "--base-url", "http://h/")
        self.assertEqual(code, 1, out)
        self.assertIn("FAIL readiness-shape:", out)
        self.assertIn("/readyz 200 carries duplicate JSON object keys", out)

    def test_unique_nested_objects_still_parse(self):
        _, raw = (200, json.dumps({
            "components": [["process", "ready"], ["gateway", "ready"]],
            "jobs": {"counter": {"parked": False, "running": True}},
            "build_revision": "0123456789abcdef0123456789abcdef01234567", "build_id": "700-1",
        }).encode())
        state, _ = probe.check_readyz_shape(200, raw)
        self.assertEqual(state["gateway"], "ready")

    def test_escaped_key_spelling_counts_as_duplicate(self):
        raw = b'{"status": "ok", "\\u0073tatus": "ok"}'
        with self.assertRaises(probe.ProbeError) as ctx:
            probe.check_liveness(200, raw)
        self.assertEqual(str(ctx.exception),
                         "/health carries duplicate JSON object keys")

    def test_diagnostic_echoes_no_key_or_value(self):
        sentinel = "SENTINEL_dupkey_cutover_abc123"
        raw = ('{"components": ' + self.READY_PAIR
               + ', "jobs": {"counter": {"parked": true, "running": false}}, '
               + '"build_revision": "0123456789abcdef0123456789abcdef01234567", "build_id": "700-1", '
               + json.dumps(sentinel) + ': 1, ' + json.dumps(sentinel) + ': 2}').encode()
        with self.assertRaises(probe.ProbeError) as ctx:
            probe.check_readyz_shape(200, raw)
        self.assertEqual(str(ctx.exception),
                         "/readyz 200 carries duplicate JSON object keys")
        self.assertNotIn(sentinel, str(ctx.exception))
        code, out = run(double({"/health": HEALTH, "/readyz": (200, raw)}),
                        "--base-url", "http://h/")
        self.assertEqual(code, 1, out)
        self.assertNotIn(sentinel, out)

    def test_duplicate_writes_no_receipt(self):
        raw = ('{"components": ' + self.DOWN_PAIR
               + ', "jobs": {"counter": {"parked": true, "running": false}}, '
               + '"build_revision": "0123456789abcdef0123456789abcdef01234567", "build_id": "700-1", '
               + '"components": ' + self.READY_PAIR + '}').encode()
        with mock.patch("builtins.open") as handle:
            code, _ = run(double({"/health": HEALTH, "/readyz": (200, raw)}),
                          "--base-url", "http://h/",
                          "--evidence", "receipt.json")
        self.assertEqual(code, 1)
        handle.assert_not_called()

    def test_health_duplicate_writes_no_receipt(self):
        raw = b'{"status": "ok", "status": "ok"}'
        with mock.patch("builtins.open") as handle:
            code, out = run(double({"/health": (200, raw), "/readyz": PARKED}),
                            "--base-url", "http://h/",
                            "--evidence", "receipt.json")
        self.assertEqual(code, 1, out)
        self.assertIn("/health carries duplicate JSON object keys", out)
        handle.assert_not_called()


class UnknownComponentTest(unittest.TestCase):
    def unknown_body(self, extra, status=200):
        pairs = [("process", "ready"), ("gateway", "ready"), extra]
        return (status, json.dumps({
            "components": [[name, value] for name, value in pairs],
            "jobs": {"counter": {"parked": True, "running": False}},
            "build_revision": "0123456789abcdef0123456789abcdef01234567", "build_id": "700-1",
        }).encode())

    def test_unknown_ready_refused(self):
        _, raw = self.unknown_body(("voice", "ready"), status=200)
        with self.assertRaisesRegex(probe.ProbeError, "unknown component name"):
            probe.check_readyz_shape(200, raw)

    def test_unknown_down_refused(self):
        _, raw = self.unknown_body(("voice", "down"), status=503)
        with self.assertRaisesRegex(probe.ProbeError, "unknown component name"):
            probe.check_readyz_shape(503, raw)

    def test_unknown_starting_refused(self):
        _, raw = self.unknown_body(("voice", "starting"), status=503)
        with self.assertRaisesRegex(probe.ProbeError, "unknown component name"):
            probe.check_readyz_shape(503, raw)

    def test_diagnostic_echoes_no_component_data(self):
        sentinel = "SENTINEL_unknown_cutover_abc123"
        _, raw = self.unknown_body((sentinel, "ready"), status=200)
        with self.assertRaises(probe.ProbeError) as ctx:
            probe.check_readyz_shape(200, raw)
        self.assertIn("unknown component name", str(ctx.exception))
        self.assertNotIn(sentinel, str(ctx.exception))

    def test_legitimate_optional_components_still_parse(self):
        for pairs, status in (
            ([("process", "ready"), ("gateway", "ready")], 200),
            ([("process", "ready"), ("gateway", "ready"),
              ("database", "ready")], 200),
            ([("process", "ready"), ("gateway", "ready"),
              ("database", "ready"), ("token_invalid", "ready")], 200),
            ([("process", "ready"), ("gateway", "down"),
              ("database", "down"), ("token_invalid", "ready")], 503),
        ):
            with self.subTest(pairs=pairs):
                _, raw = (status, json.dumps({
                    "components": [[n, v] for n, v in pairs],
                    "jobs": {"counter": {"parked": True, "running": False}},
                    "build_revision": "0123456789abcdef0123456789abcdef01234567", "build_id": "700-1",
                }).encode())
                state, _ = probe.check_readyz_shape(status, raw)
                self.assertEqual(set(state), {n for n, _ in pairs})

    def test_unknown_fails_readiness_shape_end_to_end(self):
        body = self.unknown_body(("voice", "ready"), status=200)
        code, out = run(double({"/health": HEALTH, "/readyz": body}),
                        "--base-url", "http://h/")
        self.assertEqual(code, 1)
        self.assertIn("FAIL readiness-shape:", out)
        self.assertIn("unknown component name", out)
        self.assertNotIn("voice", out)


class EvidenceTest(unittest.TestCase):
    def test_pass_writes_receipt_with_sources(self):
        with mock.patch("builtins.open", mock.mock_open()) as handle:
            code, _ = run(double({"/health": HEALTH, "/readyz": PARKED}),
                          "--base-url", "http://h/",
                          "--evidence", "receipt.json")
        self.assertEqual(code, 0)
        written = "".join(c.args[0] for c in handle().write.call_args_list)
        receipt = json.loads(written)
        self.assertEqual(receipt["result"], "pass")
        self.assertEqual(len(receipt["checks"]), 5)
        self.assertTrue(all(c["source"] for c in receipt["checks"]))

    def test_fail_writes_no_receipt(self):
        with mock.patch("builtins.open") as handle:
            code, _ = run(double({"/health": HEALTH, "/readyz": FENCED}),
                          "--base-url", "http://h/",
                          "--evidence", "receipt.json")
        self.assertEqual(code, 1)
        handle.assert_not_called()

    def test_json_evidence_failure_is_fail_verdict_with_empty_stderr(self):
        out, err = io.StringIO(), io.StringIO()
        with mock.patch("builtins.open",
                        side_effect=IsADirectoryError(21, "Is a directory")), \
                redirect_stdout(out), redirect_stderr(err):
            code = probe.main(["--base-url", "http://h/", "--json",
                               "--evidence", "receipt.json"],
                              fetch_fn=double({"/health": HEALTH,
                                               "/readyz": READY}))
        self.assertEqual(code, 1)
        self.assertEqual(err.getvalue(), "")
        report = json.loads(out.getvalue())
        self.assertEqual(report["verdict"], "FAIL")
        rows = {row["id"]: row for row in report["checks"]}
        self.assertEqual(rows["evidence"]["verdict"], "FAIL")
        self.assertEqual(rows["evidence"]["code"], "evidence_not_written")
        self.assertIn("IsADirectoryError", rows["evidence"]["detail"])
        self.assertTrue(all(row["verdict"] == "PASS"
                            for row in report["checks"]
                            if row["id"] != "evidence"))

    def test_text_evidence_failure_keeps_notice_on_stdout(self):
        out, err = io.StringIO(), io.StringIO()
        with mock.patch("builtins.open",
                        side_effect=PermissionError(13, "denied")), \
                redirect_stdout(out), redirect_stderr(err):
            code = probe.main(["--base-url", "http://h/",
                               "--evidence", "receipt.json"],
                              fetch_fn=double({"/health": HEALTH,
                                               "/readyz": READY}))
        self.assertEqual(code, 1)
        self.assertIn("evidence not written (PermissionError)",
                      out.getvalue())
        self.assertEqual(err.getvalue(), "")


class UserAgentTests(unittest.TestCase):
    def test_fetch_sends_an_explicit_user_agent(self):
        seen = []

        class Response(io.BytesIO):
            status = 200

            def __enter__(self):
                return self

            def __exit__(self, *exc):
                return False

        class Opener:
            def open(self, request, timeout=None):
                seen.append(request.get_header("User-agent"))
                return Response(b"{}")

        with mock.patch.object(probe.urllib.request, "build_opener", return_value=Opener()):
            probe.fetch("https://two-bot-next-staging.example-sub.workers.dev/readyz", 10)
        self.assertEqual(seen, [probe.USER_AGENT])
        self.assertFalse(seen[0].startswith("Python-urllib"))


def readyz_with_jobs(jobs, status=200):
    """Build a /readyz double from {name: (parked, age_seconds_or_None)}.

    age None omits last_success (a job with no success recorded yet). A
    3-tuple (parked, age, extra_dict) merges extra status fields such as
    consecutive_failures, last_error_class or last_start into the entry.
    """
    now_ms = int(time.time() * 1000)
    payload = {
        "components": [["process", "ready"], ["gateway", "ready"]],
        "jobs": {},
        "build_revision": "0123456789abcdef0123456789abcdef01234567", "build_id": "700-1",
    }
    for name, spec in jobs.items():
        parked, age = spec[0], spec[1]
        extra = spec[2] if len(spec) > 2 else {}
        entry = {"parked": parked, "running": not parked}
        if age is not None:
            entry["last_success"] = now_ms - age * 1000
        for key, value in extra.items():
            if key == "start_age" and value is not None:
                entry["last_start"] = now_ms - value * 1000
            else:
                entry[key] = value
        payload["jobs"][name] = entry
    return (status, json.dumps(payload).encode())


class CadenceTest(unittest.TestCase):
    def test_one_stale_job_fails_and_names_it(self):
        body = readyz_with_jobs({
            "counter": (False, 10),
            "rank": (False, 1400),  # fail line is 1320 s (2x600 + 120)
        })
        code, out = run(double({"/health": HEALTH, "/readyz": body}),
                        "--base-url", "http://h/")
        self.assertEqual(code, 1, out)
        self.assertIn("FAIL jobs-map:", out)
        self.assertIn("rank=", out)
        self.assertIn("stale", out)

    def test_healthy_sample_just_inside_cadence_passes(self):
        # Every graded job sampled at 0.95x its real cadence (plus no
        # runtime yet) must PASS: bands sit above 1x cadence + timeout.
        jobs = {name: (False, int(0.95 * cadence))
                for name, cadence in probe.JOB_CADENCE_SECONDS.items()}
        body = readyz_with_jobs(jobs)
        code, out = run(double({"/health": HEALTH, "/readyz": body}),
                        "--base-url", "http://h/")
        self.assertEqual(code, 0, out)
        self.assertIn("PASS jobs-map:", out)

    def test_tabletop_baseline_passes(self):
        body = readyz_with_jobs({
            "counter": (False, 50),
            "member_unban_sweep": (False, 50),
            "scheduled_messages": (False, 50),
            "settings": (False, 10),
            "rank": (False, 350),
            "scheduled_events": (False, 350),
            "inactivity": (False, 3300),
            "presence_probe": (False, 3300),
        })
        code, out = run(double({"/health": HEALTH, "/readyz": body}),
                        "--base-url", "http://h/")
        self.assertEqual(code, 0, out)
        self.assertIn("PASS jobs-map:", out)

    def test_env_configured_feeds_is_not_graded(self):
        # feeds cadence is env-configured (TWO_FEED_POLL_SECONDS), so even
        # an ancient sample must not fail the probe.
        body = readyz_with_jobs({
            "feeds": (False, 10 ** 6),
        })
        code, out = run(double({"/health": HEALTH, "/readyz": body}),
                        "--base-url", "http://h/")
        self.assertEqual(code, 0, out)
        self.assertIn("PASS jobs-map:", out)

    def test_warn_band_stays_green_but_is_named(self):
        body = readyz_with_jobs({
            "rank": (False, 800),  # warn 720 s, fail 1320 s
        })
        code, out = run(double({"/health": HEALTH, "/readyz": body}),
                        "--base-url", "http://h/")
        self.assertEqual(code, 0, out)
        self.assertIn("PASS jobs-map:", out)
        self.assertIn("warn", out)
        self.assertIn("rank=", out)

    def test_never_succeeded_with_failures_fails_and_names_it(self):
        body = readyz_with_jobs({
            "counter": (False, None, {"consecutive_failures": 40,
                                      "last_error_class": "database"}),
        })
        code, out = run(double({"/health": HEALTH, "/readyz": body}),
                        "--base-url", "http://h/")
        self.assertEqual(code, 1, out)
        self.assertIn("FAIL jobs-map:", out)
        self.assertIn("counter", out)
        self.assertIn("consecutive failures", out)

    def test_never_succeeded_without_attempts_passes(self):
        now_ms = int(time.time() * 1000)
        body = (200, json.dumps({
            "components": [["process", "ready"], ["gateway", "ready"]],
            "jobs": {"counter": {"parked": False, "running": True,
                                 "consecutive_failures": 0,
                                 "last_start": now_ms - 5_000}},
            "build_revision": "0123456789abcdef0123456789abcdef01234567", "build_id": "700-1",
        }).encode())
        code, out = run(double({"/health": HEALTH, "/readyz": body}),
                        "--base-url", "http://h/")
        self.assertEqual(code, 0, out)
        self.assertIn("PASS jobs-map:", out)

    def test_stale_last_start_without_success_fails(self):
        body = readyz_with_jobs({
            # counter fail line is 165 s; started 1000 s ago, never succeeded
            "counter": (False, None, {"consecutive_failures": 0,
                                      "start_age": 1000}),
        })
        code, out = run(double({"/health": HEALTH, "/readyz": body}),
                        "--base-url", "http://h/")
        self.assertEqual(code, 1, out)
        self.assertIn("FAIL jobs-map:", out)
        self.assertIn("counter", out)
        self.assertIn("last_start", out)

    def test_parked_job_with_ancient_success_passes(self):
        body = readyz_with_jobs({
            "inactivity": (True, 10 ** 6),
        })
        code, out = run(double({"/health": HEALTH, "/readyz": body}),
                        "--base-url", "http://h/")
        self.assertEqual(code, 0, out)
        self.assertIn("PASS jobs-map:", out)

    def test_unknown_job_is_not_graded(self):
        body = readyz_with_jobs({
            "brand_new_job": (False, 10 ** 6),
        })
        code, out = run(double({"/health": HEALTH, "/readyz": body}),
                        "--base-url", "http://h/")
        self.assertEqual(code, 0, out)
        self.assertIn("PASS jobs-map:", out)

    def test_missing_last_success_is_not_evidence(self):
        body = readyz_with_jobs({
            "counter": (False, None),
        })
        code, out = run(double({"/health": HEALTH, "/readyz": body}),
                        "--base-url", "http://h/")
        self.assertEqual(code, 0, out)
        self.assertIn("PASS jobs-map:", out)

    def test_future_timestamp_clamps_to_zero(self):
        now_ms = int(time.time() * 1000)
        body = (200, json.dumps({
            "components": [["process", "ready"], ["gateway", "ready"]],
            "jobs": {"counter": {"parked": False, "running": True,
                                 "last_success": now_ms + 60_000}},
            "build_revision": "0123456789abcdef0123456789abcdef01234567", "build_id": "700-1",
        }).encode())
        code, out = run(double({"/health": HEALTH, "/readyz": body}),
                        "--base-url", "http://h/")
        self.assertEqual(code, 0, out)
        self.assertIn("PASS jobs-map:", out)

    def test_degraded_503_with_failing_jobs_stays_green(self):
        # Truthful 503 (database down) without --expect-ready grades
        # truthfulness, not scheduler progress: failing jobs stay green.
        now_ms = int(time.time() * 1000)
        body = (503, json.dumps({
            "components": [["process", "ready"], ["gateway", "ready"],
                           ["database", "down"]],
            "jobs": {"counter": {"parked": False, "running": True,
                                 "consecutive_failures": 3,
                                 "last_error_class": "database",
                                 "last_start": now_ms - 5_000}},
            "build_revision": "0123456789abcdef0123456789abcdef01234567", "build_id": "700-1",
        }).encode())
        code, out = run(double({"/health": HEALTH, "/readyz": body}),
                        "--base-url", "http://h/")
        self.assertEqual(code, 0, out)
        self.assertIn("PASS jobs-map:", out)
        self.assertIn("freshness not graded", out)

    def test_degraded_503_with_failing_jobs_fails_expect_ready(self):
        # The strict gate grades freshness too: the same degraded report
        # fails jobs-map once --expect-ready is set.
        now_ms = int(time.time() * 1000)
        body = (503, json.dumps({
            "components": [["process", "ready"], ["gateway", "ready"],
                           ["database", "down"]],
            "jobs": {"counter": {"parked": False, "running": True,
                                 "consecutive_failures": 3,
                                 "last_error_class": "database",
                                 "last_start": now_ms - 5_000}},
            "build_revision": "0123456789abcdef0123456789abcdef01234567", "build_id": "700-1",
        }).encode())
        code, out = run(double({"/health": HEALTH, "/readyz": body}),
                        "--base-url", "http://h/", "--expect-ready")
        self.assertEqual(code, 1, out)
        self.assertIn("FAIL jobs-map:", out)
        self.assertIn("counter", out)

    def test_mixed_report_counts_are_honest(self):
        # Only jobs graded against a fresh last_success count as fresh;
        # ungraded (feeds/unknown) and not-yet-run jobs are reported
        # separately, and the receipt must not overstate verification.
        body = readyz_with_jobs({
            "feeds": (False, 10 ** 6),
            "brand_new_job": (False, 10 ** 6),
            "counter": (False, None),
            "rank": (False, 10),
            "inactivity": (True, 10 ** 6),
        })
        code, out = run(double({"/health": HEALTH, "/readyz": body}),
                        "--base-url", "http://h/")
        self.assertEqual(code, 0, out)
        self.assertIn("PASS jobs-map:", out)
        self.assertIn("1 jobs fresh within cadence", out)
        self.assertIn("2 ungraded", out)
        self.assertNotIn("brand_new_job", out)
        self.assertIn("1 not yet run", out)
        self.assertIn("1 parked", out)
        self.assertNotIn("4 jobs fresh", out)


class NonfiniteJobFieldTest(unittest.TestCase):
    """Nonfinite numeric job fields fail as structured jobs-map FAILs.

    Raw NaN/Infinity tokens and overflowing exponents (which decode to
    Infinity) must never traceback nor count as verified fresh; they fail
    the jobs-map check with bounded diagnostics naming the job and field.
    """

    def raw_body(self, counter_entry_raw):
        return (200, (
            '{"components": [["process", "ready"], ["gateway", "ready"]],'
            '"jobs": {"counter": ' + counter_entry_raw + '},'
            '"build_revision": "0123456789abcdef0123456789abcdef01234567", "build_id": "700-1"}').encode())

    def assert_structured_fail(self, counter_entry_raw, field):
        body = self.raw_body(counter_entry_raw)
        code, out = run(double({"/health": HEALTH, "/readyz": body}),
                        "--base-url", "http://h/")
        self.assertEqual(code, 1, out)
        self.assertIn("FAIL jobs-map:", out)
        self.assertIn("counter", out)
        self.assertIn("nonfinite", out.lower())
        self.assertIn(field, out)
        self.assertNotIn("Traceback", out)
        self.assertNotIn("jobs fresh within cadence", out)
        return out

    def test_nan_last_success_fails(self):
        self.assert_structured_fail(
            '{"parked": false, "running": true, "last_success": NaN}',
            "last_success")

    def test_infinity_last_success_fails(self):
        self.assert_structured_fail(
            '{"parked": false, "running": true, "last_success": Infinity}',
            "last_success")

    def test_negative_infinity_last_success_fails(self):
        self.assert_structured_fail(
            '{"parked": false, "running": true, "last_success": -Infinity}',
            "last_success")

    def test_overflow_exponent_last_success_fails(self):
        self.assert_structured_fail(
            '{"parked": false, "running": true, "last_success": 1e10000}',
            "last_success")

    def test_nan_last_start_without_success_fails(self):
        self.assert_structured_fail(
            '{"parked": false, "running": true, '
            '"consecutive_failures": 0, "last_start": NaN}',
            "last_start")

    def test_infinity_last_start_without_success_fails(self):
        self.assert_structured_fail(
            '{"parked": false, "running": true, '
            '"consecutive_failures": 0, "last_start": Infinity}',
            "last_start")

    def test_nan_consecutive_failures_fails(self):
        self.assert_structured_fail(
            '{"parked": false, "running": true, '
            '"consecutive_failures": NaN}',
            "consecutive_failures")

    def test_infinity_consecutive_failures_fails(self):
        self.assert_structured_fail(
            '{"parked": false, "running": true, '
            '"consecutive_failures": Infinity}',
            "consecutive_failures")

    def test_overflow_exponent_consecutive_failures_fails(self):
        self.assert_structured_fail(
            '{"parked": false, "running": true, '
            '"consecutive_failures": 1e10000}',
            "consecutive_failures")

    def test_nonfinite_failures_with_fresh_success_still_fails(self):
        now_ms = int(time.time() * 1000)
        entry = ('{"parked": false, "running": true, '
                 '"last_success": %d, "consecutive_failures": NaN}' % now_ms)
        self.assert_structured_fail(entry, "consecutive_failures")

    def test_nonfinite_leaves_no_receipt(self):
        body = self.raw_body(
            '{"parked": false, "running": true, "last_success": NaN}')
        with mock.patch("builtins.open") as handle:
            code, out = run(double({"/health": HEALTH, "/readyz": body}),
                            "--base-url", "http://h/",
                            "--evidence", "receipt.json")
        self.assertEqual(code, 1, out)
        self.assertIn("FAIL jobs-map:", out)
        handle.assert_not_called()

    def test_parked_job_with_nonfinite_stays_green(self):
        body = self.raw_body(
            '{"parked": true, "running": false, "last_success": NaN}')
        code, out = run(double({"/health": HEALTH, "/readyz": body}),
                        "--base-url", "http://h/")
        self.assertEqual(code, 0, out)
        self.assertIn("PASS jobs-map:", out)

    def test_ungraded_feeds_with_nonfinite_stays_green(self):
        body = (200, (
            '{"components": [["process", "ready"], ["gateway", "ready"]],'
            '"jobs": {"feeds": {"parked": false, "running": true, '
            '"last_success": NaN}},'
            '"build_revision": "0123456789abcdef0123456789abcdef01234567", "build_id": "700-1"}').encode())
        code, out = run(double({"/health": HEALTH, "/readyz": body}),
                        "--base-url", "http://h/")
        self.assertEqual(code, 0, out)
        self.assertIn("PASS jobs-map:", out)

    def test_ungraded_unknown_job_with_nonfinite_stays_green(self):
        body = (200, (
            '{"components": [["process", "ready"], ["gateway", "ready"]],'
            '"jobs": {"brand_new_job": {"parked": false, "running": true, '
            '"consecutive_failures": Infinity}},'
            '"build_revision": "0123456789abcdef0123456789abcdef01234567", "build_id": "700-1"}').encode())
        code, out = run(double({"/health": HEALTH, "/readyz": body}),
                        "--base-url", "http://h/")
        self.assertEqual(code, 0, out)
        self.assertIn("PASS jobs-map:", out)

    def test_degraded_503_with_nonfinite_stays_green(self):
        body = (503, (
            '{"components": [["process", "ready"], ["gateway", "ready"], '
            '["database", "down"]],'
            '"jobs": {"counter": {"parked": false, "running": true, '
            '"last_success": NaN}},'
            '"build_revision": "0123456789abcdef0123456789abcdef01234567", "build_id": "700-1"}').encode())
        code, out = run(double({"/health": HEALTH, "/readyz": body}),
                        "--base-url", "http://h/")
        self.assertEqual(code, 0, out)
        self.assertIn("PASS jobs-map:", out)
        self.assertIn("freshness not graded", out)

    def test_fresh_success_with_nonfinite_last_start_still_fails(self):
        now_ms = int(time.time() * 1000)
        entry = ('{"parked": false, "running": true, '
                 '"last_success": %d, "last_start": NaN}' % now_ms)
        self.assert_structured_fail(entry, "last_start")

    def test_helpers_reject_nonfinite_directly(self):
        now_ms = int(time.time() * 1000)
        for field in ("last_success", "last_start"):
            for value in (float("nan"), float("inf"), float("-inf")):
                with self.subTest(field=field, value=value):
                    with self.assertRaisesRegex(probe.ProbeError,
                                                f"nonfinite {field}"):
                        probe._field_age_seconds({field: value}, field, now_ms)
        for value in (float("nan"), float("inf"), float("-inf")):
            with self.subTest(value=value):
                with self.assertRaisesRegex(probe.ProbeError,
                                            "nonfinite consecutive_failures"):
                    probe.consecutive_failures({"consecutive_failures": value})


class HugeMagnitudeTest(unittest.TestCase):
    """Absurd finite magnitudes are display-capped, never expanded digit
    for digit: an in-cap body must stay within the 16 KiB machine bound."""

    HUGE = 10 ** 4200

    def test_helpers_cap_absurd_magnitudes_without_changing_grading(self):
        now_ms = int(time.time() * 1000)
        self.assertEqual(
            probe._field_age_seconds({"last_success": -self.HUGE},
                                     "last_success", now_ms),
            probe.MAX_DISPLAY_AGE_SECONDS)
        self.assertEqual(
            probe.consecutive_failures(
                {"consecutive_failures": self.HUGE}),
            probe.MAX_DISPLAY_FAILURES)
        # Realistic values flow through untouched.
        self.assertEqual(
            probe._field_age_seconds({"last_success": now_ms - 5000},
                                     "last_success", now_ms), 5)
        self.assertEqual(
            probe.consecutive_failures({"consecutive_failures": 4}), 4)

    def test_huge_remote_magnitudes_stay_within_output_bound(self):
        jobs = {}
        for index, name in enumerate(probe.JOB_CADENCE_SECONDS):
            if index % 3 == 0:
                jobs[name] = {"parked": False, "running": True,
                              "last_success": -self.HUGE}
            elif index % 3 == 1:
                jobs[name] = {"parked": False, "running": True,
                              "consecutive_failures": self.HUGE}
            else:
                jobs[name] = {"parked": False, "running": True,
                              "last_start": -self.HUGE}
        body = (200, json.dumps({
            "components": [["process", "ready"], ["gateway", "ready"]],
            "jobs": jobs,
            "build_revision": "0123456789abcdef0123456789abcdef01234567",
            "build_id": "700-1",
        }).encode())
        self.assertLessEqual(len(body[1]), probe.BODY_CAP)
        for flags in (["--json"], []):
            with self.subTest(flags=flags):
                out, err = io.StringIO(), io.StringIO()
                with redirect_stdout(out), redirect_stderr(err):
                    code = probe.main(["--base-url", "http://h/", *flags],
                                      fetch_fn=double({"/health": HEALTH,
                                                       "/readyz": body}))
                self.assertEqual(code, 1)
                combined = out.getvalue() + err.getvalue()
                self.assertLessEqual(len(combined.encode()), 16_384)
                self.assertNotIn("Traceback", combined)


class DecoderRecursionTest(unittest.TestCase):
    """A decoder RecursionError becomes None (an ordinary failed check),
    never a traceback escaping the verdict path."""

    def test_decode_returns_none_when_decoder_recurses(self):
        with mock.patch.object(probe, "decode_strict",
                               side_effect=RecursionError("max depth")):
            self.assertIsNone(probe.decode(b"[0]"))

    def test_recursing_body_fails_a_check_without_traceback(self):
        out, err = io.StringIO(), io.StringIO()
        with mock.patch.object(probe, "decode_strict",
                               side_effect=RecursionError("max depth")), \
                redirect_stdout(out), redirect_stderr(err):
            code = probe.main(["--base-url", "http://h/", "--json"],
                              fetch_fn=double({"/health": HEALTH,
                                               "/readyz": READY}))
        self.assertEqual(code, 1)
        self.assertEqual(err.getvalue(), "")
        report = json.loads(out.getvalue())
        self.assertEqual(report["verdict"], "FAIL")
        self.assertNotIn("Traceback", out.getvalue())

class FixedCadenceJobGradingTest(unittest.TestCase):
    """self_role_recovery (30 s cadence, 25 s timeout) and automod_expiry
    (60 s cadence, 5 s timeout) have fixed cadences, so they are graded with
    the same cadence+timeout / 2x cadence+timeout bands as their siblings."""

    # name: (warn line, max line), pinned from the Rust constants.
    BANDS = {
        "self_role_recovery": (55, 85),
        "automod_expiry": (65, 125),
    }

    def probe(self, jobs):
        return run(double({"/health": HEALTH, "/readyz": readyz_with_jobs(jobs)}),
                   "--base-url", "http://h/")

    def test_bands_derive_from_cadence_and_timeout(self):
        for name, (warn, limit) in self.BANDS.items():
            self.assertEqual(probe.WARN_LAST_SUCCESS_AGE_SECONDS[name], warn, name)
            self.assertEqual(probe.MAX_LAST_SUCCESS_AGE_SECONDS[name], limit, name)

    def test_cadence_and_timeout_maps_cover_the_same_jobs(self):
        self.assertEqual(set(probe.JOB_CADENCE_SECONDS),
                         set(probe.JOB_TIMEOUT_SECONDS))

    def test_fresh_samples_pass_without_a_warning(self):
        code, out = self.probe({name: (False, 10) for name in self.BANDS})
        self.assertEqual(code, 0, out)
        self.assertIn("PASS jobs-map:", out)
        self.assertIn("2 jobs fresh within cadence", out)
        self.assertNotIn("ungraded", out)
        self.assertNotIn("warn", out)

    def test_between_warn_and_max_stays_green_but_is_named(self):
        for name, (warn, limit) in self.BANDS.items():
            with self.subTest(name=name):
                code, out = self.probe({name: (False, warn + 10)})
                self.assertEqual(code, 0, out)
                self.assertIn("PASS jobs-map:", out)
                self.assertIn(f"[warn: {name}=", out)

    def test_past_max_fails_and_names_the_job_and_limit(self):
        for name, (warn, limit) in self.BANDS.items():
            with self.subTest(name=name):
                code, out = self.probe({"counter": (False, 10),
                                        name: (False, limit + 10)})
                self.assertEqual(code, 1, out)
                self.assertIn("FAIL jobs-map:", out)
                self.assertIn(f"{name}=", out)
                self.assertIn(f"> {limit}s", out)
                self.assertNotIn("counter=", out)

    def test_never_succeeded_with_failures_fails_and_names_the_job(self):
        for name in self.BANDS:
            with self.subTest(name=name):
                code, out = self.probe({name: (False, None, {
                    "consecutive_failures": 4, "last_error_class": "database"})})
                self.assertEqual(code, 1, out)
                self.assertIn(f"{name}=no-success 4 consecutive failures", out)

    def test_stale_last_start_without_success_fails(self):
        for name, (warn, limit) in self.BANDS.items():
            with self.subTest(name=name):
                code, out = self.probe({name: (False, None, {
                    "consecutive_failures": 0, "start_age": limit + 60})})
                self.assertEqual(code, 1, out)
                self.assertIn(f"{name}=last_start", out)

    def test_never_run_yet_is_not_evidence(self):
        code, out = self.probe({name: (False, None) for name in self.BANDS})
        self.assertEqual(code, 0, out)
        self.assertIn("2 not yet run", out)

    def test_parked_with_ancient_success_passes(self):
        code, out = self.probe({name: (True, 10 ** 6) for name in self.BANDS})
        self.assertEqual(code, 0, out)
        self.assertIn("2 parked", out)

    def test_absent_from_the_map_is_not_a_failure(self):
        # Absence (an older build, or the job not registered) is unchanged:
        # the probe grades only what /readyz reports.
        code, out = self.probe({"counter": (False, 10)})
        self.assertEqual(code, 0, out)
        self.assertIn("1 jobs fresh within cadence", out)


def counting_double():
    inner = double({"/health": HEALTH, "/readyz": READY})
    calls = []

    def fetch(url):
        calls.append(url)
        return inner(url)

    fetch.calls = calls
    return fetch


class TimeoutCapBoundaryTests(unittest.TestCase):
    """Pin the documented 60 s --timeout cap boundary in this suite.

    The acceptance fixtures stop valid values at 30 and only refuse 1e9
    over the cap, so a strict-< off-by-one or a raised cap would survive
    there. These two cases sit exactly at the bound and just above it.
    """

    def test_timeout_cap_60_fetches_both_routes(self):
        fetch = counting_double()
        out, err = io.StringIO(), io.StringIO()
        with redirect_stdout(out), redirect_stderr(err):
            code = probe.main(["--base-url", "http://h/", "--timeout", "60"],
                              fetch_fn=fetch)
        self.assertEqual(code, 0, out.getvalue() + err.getvalue())
        self.assertEqual(len(fetch.calls), 2, "both routes must be fetched")

    def test_timeout_just_over_cap_61_refuses_before_any_request(self):
        fetch = counting_double()
        out, err = io.StringIO(), io.StringIO()
        with redirect_stdout(out), redirect_stderr(err):
            code = probe.main(["--base-url", "http://h/", "--timeout", "61"],
                              fetch_fn=fetch)
        self.assertEqual(code, 2)
        self.assertEqual(fetch.calls, [], "a refusal must attempt zero requests")
        combined = out.getvalue() + err.getvalue()
        self.assertIn("--timeout", combined)
        self.assertIn("at most 60", combined)
        self.assertNotIn("PASS", combined, "a refusal must not print check lines")
        self.assertNotIn("FAIL", combined, "a refusal must not print check lines")


class BodyCapRefusalTests(unittest.TestCase):
    """Oversized bodies are refused before JSON parsing (M4.42)."""

    def padded(self, value, size):
        raw = json.dumps(value).encode()
        self.assertLessEqual(len(raw), size, "fixture must fit the target size")
        return raw + b" " * (size - len(raw))

    def test_exact_cap_valid_input_passes(self):
        health = self.padded({"status": "ok"}, probe.BODY_CAP)
        self.assertEqual(len(health), probe.BODY_CAP)
        code, out = run(double({"/health": (200, health), "/readyz": PARKED}),
                        "--base-url", "http://h/")
        self.assertEqual(code, 0, out)
        self.assertIn("PASS liveness:", out)
        self.assertIn("PASS readiness-shape:", out)

    def test_valid_json_padded_past_cap_refuses_before_parsing(self):
        sentinel = "SENTINEL_BODY_cutover_padded"
        base = json.dumps({"status": "ok", "note": sentinel}).encode()
        health = base + b" " * (probe.BODY_CAP + 1 - len(base))
        self.assertEqual(len(health), probe.BODY_CAP + 1)
        self.assertEqual(json.loads(health)["note"], sentinel)
        code, out = run(double({"/health": (200, health), "/readyz": PARKED}),
                        "--base-url", "http://h/")
        self.assertEqual(code, 1, out)
        self.assertIn("FAIL liveness:", out)
        self.assertIn("/health answered over the body cap", out)
        self.assertNotIn(sentinel, out)

    def test_cap_length_valid_prefix_with_hidden_tail_refuses(self):
        prefix = self.padded({"status": "ok"}, probe.BODY_CAP)
        self.assertEqual(json.loads(prefix), {"status": "ok"})
        code, out = run(double({"/health": (200, prefix + b"X"), "/readyz": PARKED}),
                        "--base-url", "http://h/")
        self.assertEqual(code, 1, out)
        self.assertIn("FAIL liveness:", out)
        self.assertIn("/health answered over the body cap", out)

    def test_readyz_oversize_never_reaches_json_and_reports_no_breakdown(self):
        sentinel = "SENTINEL_BODY_readyz_oversize"
        payload = {"components": [["process", "ready"], ["gateway", "ready"]],
                   "jobs": {"counter": {"parked": True, "running": False}},
                   "build_revision": "0123456789abcdef0123456789abcdef01234567", "build_id": "700-1", "note": sentinel}
        base = json.dumps(payload).encode()
        big = base + b" " * (probe.BODY_CAP + 1 - len(base))
        self.assertEqual(json.loads(big)["note"], sentinel)
        code, out = run(double({"/health": HEALTH, "/readyz": (200, big)}),
                        "--base-url", "http://h/")
        self.assertEqual(code, 1, out)
        self.assertIn("FAIL readiness-shape:", out)
        self.assertIn("/readyz answered over the body cap", out)
        self.assertIn("FAIL gateway-state:", out)
        self.assertIn("no truthful breakdown to read gateway from", out)
        self.assertIn("FAIL jobs-map:", out)
        self.assertIn("FAIL fence-watch:", out)
        self.assertNotIn(sentinel, out)

    def test_httperror_oversize_enforces_the_same_bound(self):
        # The real fetch converts an HTTPError into (status, body); an
        # oversized 503 body must still refuse with the cap message, never a
        # breakdown verdict computed from parsed JSON.
        payload = {"components": [["process", "ready"], ["gateway", "ready"]],
                   "jobs": {"counter": {"parked": True, "running": False}},
                   "build_revision": "0123456789abcdef0123456789abcdef01234567", "build_id": "700-1"}
        big = self.padded(payload, probe.BODY_CAP + 1)
        self.assertEqual(len(big), probe.BODY_CAP + 1)
        code, out = run(double({"/health": HEALTH, "/readyz": (503, big)}),
                        "--base-url", "http://h/")
        self.assertEqual(code, 1, out)
        self.assertIn("/readyz answered over the body cap", out)

    def test_transport_reads_bounded_cap_plus_one_both_paths(self):
        import urllib.error
        seen = []

        class Response(io.BytesIO):
            status = 200

            def __init__(self, payload):
                super().__init__(payload)

            def read(self, n=-1):
                seen.append(n)
                return super().read(n)

            def __enter__(self):
                return self

            def __exit__(self, *exc):
                return False

        class OkOpener:
            def open(self, request, timeout=None):
                return Response(b"{}")

        with mock.patch.object(probe.urllib.request, "build_opener", return_value=OkOpener()):
            probe.fetch("http://h/health", 5)
        self.assertEqual(seen, [probe.BODY_CAP + 1])
        seen.clear()

        class ErrorOpener:
            def open(self, request, timeout=None):
                fp = Response(b"x")
                raise urllib.error.HTTPError(request.full_url, 503,
                                             "Service Unavailable", {}, fp)

        with mock.patch.object(probe.urllib.request, "build_opener",
                               return_value=ErrorOpener()):
            status, _ = probe.fetch("http://h/readyz", 5)
        self.assertEqual(status, 503)
        self.assertEqual(seen, [probe.BODY_CAP + 1])


if __name__ == "__main__":
    unittest.main()
