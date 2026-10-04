"""Staging probe regressions: a local http.server fixture only, never the network."""

import contextlib
import io
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import tempfile
import threading
import unittest
from unittest import mock

import staging_probe
from staging_probe import FenceError, fence, make_fetch, run_probes

STAGING = "https://two-bot-next-staging.5150.workers.dev"
VERSION = "f8b7cc48-cdc4-4951-b498-8b25ce7e8664"
SLUG = "probe-0123456789ab"
READY = {"components": [["process", "ready"], ["gateway", "ready"], ["database", "ready"]], "jobs": {}}
PARKED = {"components": [["process", "ready"], ["gateway", "starting"], ["database", "ready"]], "jobs": {}}


class FakeWorker(BaseHTTPRequestHandler):
    """The public contract of wrangler/src/index.ts + redirect.ts, plus overrides."""

    def log_message(self, *args):
        pass

    def parse_request(self):
        ok = super().parse_request()
        if ok:
            # http.server collapses a leading "//"; the Worker sees the raw path.
            self.path = self.requestline.split(" ")[1]
        return ok

    def do_GET(self):
        self.answer(send_body=True)

    def do_HEAD(self):
        self.answer(send_body=False)

    def answer(self, send_body):
        server = self.server
        server.seen.append({
            "method": self.command,
            "path": self.path,
            "authorization": self.headers.get("authorization"),
            "content_length": self.headers.get("content-length"),
        })
        status, headers, body = server.overrides.get(self.path) or self.route()
        payload = body.encode()
        self.send_response(status)
        for name, value in headers.items():
            self.send_header(name, value)
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        if send_body:
            self.wfile.write(payload)

    def route(self):
        path = self.path
        text = {"content-type": "text/plain"}
        if path == "/health":
            return 200, {"content-type": "application/json"}, '{"status":"ok"}'
        if path == "/readyz":
            status, body = self.server.readyz
            return status, {"content-type": "application/json"}, json.dumps(body)
        if path == "/ops/metrics":
            return 404, text, "not found"
        if path == "/healthz":
            return 200, text, "ok\n"
        if path == "/" and self.server.fallback:
            location = f"https://discord.gg/{self.server.fallback}"
            return 302, {**text, "location": location, "cache-control": "no-store, no-cache, must-revalidate"}, ""
        return 404, text, "not found\n"


class Fixture:
    def __init__(self):
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), FakeWorker)
        self.server.seen = []
        self.server.overrides = {}
        self.server.readyz = (200, READY)
        self.server.fallback = None
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.origin = f"http://127.0.0.1:{self.server.server_address[1]}"

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()


class FenceTests(unittest.TestCase):
    def test_accepts_only_the_staging_origin(self):
        for url in (STAGING, STAGING + "/", "https://TWO-BOT-NEXT-STAGING.5150.workers.dev",
                    "https://two-bot-next-staging.5150.workers.dev.", STAGING + ":443"):
            with self.subTest(url=url):
                self.assertEqual(fence(url), STAGING)

    def test_refuses_production_hosts(self):
        for url in ("https://go.two.gg", "https://GO.TWO.GG/", "https://two.gg", "https://go.two.gg.",
                    "https://staging.two.gg", "https://two-bot-next.5150.workers.dev",
                    "https://two-bot-next-production.5150.workers.dev",
                    "https://two-bot-next.other.workers.dev", "https://two-bot-next-prod.5150.workers.dev"):
            with self.subTest(url=url), self.assertRaisesRegex(FenceError, "production"):
                fence(url)

    def test_refuses_production_even_if_allowlisted(self):
        with mock.patch.object(staging_probe, "STAGING_HOSTS", frozenset({"go.two.gg"})):
            with self.assertRaisesRegex(FenceError, "production"):
                fence("https://go.two.gg")

    def test_refuses_non_https_and_non_origin_urls(self):
        for url in ("http://two-bot-next-staging.5150.workers.dev",
                    "two-bot-next-staging.5150.workers.dev",
                    "ftp://two-bot-next-staging.5150.workers.dev",
                    "https://two-bot-next-staging.5150.workers.dev:8443",
                    "https://user:pw@two-bot-next-staging.5150.workers.dev",
                    "https://two-bot-next-staging.5150.workers.dev\\@go.two.gg",
                    "https://go.two.gg@two-bot-next-staging.5150.workers.dev",
                    "https://two-bot-next-staging.5150.workers.dev/health",
                    "https://two-bot-next-staging.5150.workers.dev/?next=x",
                    "https://two-bot-next-staging.5150.workers.dev/#x",
                    "https://two-bot-next-staging.5150.workers.dev.evil.example",
                    "https://two-bot-next-staging.5150.workers.dev:99999",
                    "https://example.com", "https://[::1]", "https://127.0.0.1", "", "https://"):
            with self.subTest(url=url), self.assertRaises(FenceError):
                fence(url)

    def test_main_refuses_before_any_request(self):
        fixture = Fixture()
        self.addCleanup(fixture.close)
        for url in (fixture.origin, "https://go.two.gg", "https://two-bot-next.5150.workers.dev"):
            with self.subTest(url=url), mock.patch.object(staging_probe, "make_fetch") as transport, \
                    contextlib.redirect_stderr(io.StringIO()) as err:
                self.assertEqual(staging_probe.main(["--base-url", url, "--worker-version", VERSION]), 2)
                transport.assert_not_called()
                self.assertIn("refused", err.getvalue())
        self.assertEqual(fixture.server.seen, [])

    def test_main_rejects_invalid_arguments_before_any_request(self):
        for extra in (["--worker-version", "latest"],
                      ["--worker-version", VERSION, "--expect-fallback-code", "../evil"]):
            with self.subTest(extra=extra), mock.patch.object(staging_probe, "make_fetch") as transport, \
                    contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(staging_probe.main(["--base-url", STAGING, *extra]), 2)
                transport.assert_not_called()

    def test_transport_is_read_only(self):
        fetch = make_fetch("http://127.0.0.1:9")
        for method, path in (("POST", "/health"), ("DELETE", "/"), ("PUT", "/x"),
                             ("GET", "health"), ("GET", "https://go.two.gg/")):
            with self.subTest(method=method, path=path), self.assertRaises(ValueError):
                fetch(method, path)


class ProbeTests(unittest.TestCase):
    def setUp(self):
        self.fixture = Fixture()
        self.addCleanup(self.fixture.close)
        self.server = self.fixture.server

    def run_probes(self, **options):
        results = run_probes(make_fetch(self.fixture.origin, timeout=5), unknown_slug=SLUG, **options)
        return {r["name"]: r for r in results}

    def assertVerdicts(self, results, failing=()):
        for name, result in results.items():
            expected = "FAIL" if name in failing else "PASS"
            self.assertEqual(result["verdict"], expected, f"{name}: {result['failures']}")

    def test_contract_passes_with_read_only_requests(self):
        results = self.run_probes()
        self.assertVerdicts(results)
        self.assertEqual(results["readyz"]["observed"]["ready"], True)
        self.assertEqual(results["fallback"]["observed"], {"fallback": "unset", "unknown_slug": "not_found"})
        self.assertEqual(results["ops_metrics"]["observed"]["ops_metrics_status"], 404)
        for request in self.server.seen:
            self.assertIn(request["method"], ("GET", "HEAD"))
            self.assertIsNone(request["authorization"])
            self.assertIn(request["content_length"], (None, "0"))
        # The unknown slug is only ever asked with HEAD (never counted as a click).
        self.assertEqual([r["method"] for r in self.server.seen if r["path"] == f"/{SLUG}"], ["HEAD"])

    def test_parked_gateway_is_truthful_unless_ready_is_required(self):
        self.server.readyz = (503, PARKED)
        self.assertVerdicts(self.run_probes())
        self.assertVerdicts(self.run_probes(require_ready=True), failing={"readyz"})

    def test_untruthful_or_malformed_readyz_fails(self):
        for status, body in ((200, PARKED), (503, READY), (500, READY), (200, {"status": "ok"}),
                             (200, {"components": [["process", "fine"]]}),
                             (200, {"components": [["database", "ready"]]})):
            with self.subTest(status=status, body=body):
                self.server.readyz = (status, body)
                failing = {"readyz", "no_internal_error_text"} if status == 500 else {"readyz"}
                self.assertVerdicts(self.run_probes(), failing=failing)

    def test_configured_fallback(self):
        self.server.fallback = "twoInvite"
        results = self.run_probes(expect_fallback_code="twoInvite")
        self.assertVerdicts(results)
        self.assertEqual(results["fallback"]["observed"]["fallback_location"], "https://discord.gg/twoInvite")
        # Lookup outage: an unknown slug may redirect, but only to the fallback.
        self.server.overrides[f"/{SLUG}"] = (302, {"location": "https://discord.gg/twoInvite"}, "")
        results = self.run_probes()
        self.assertVerdicts(results)
        self.assertEqual(results["fallback"]["observed"]["unknown_slug"], "fallback")
        self.assertVerdicts(self.run_probes(expect_fallback_code="other"), failing={"fallback"})

    def test_missing_expected_fallback_fails(self):
        self.assertVerdicts(self.run_probes(expect_fallback_code="twoInvite"), failing={"fallback"})

    def test_cacheable_fallback_redirect_fails(self):
        self.server.overrides = {"/": (302, {"location": "https://discord.gg/twoInvite"}, "")}
        self.assertVerdicts(self.run_probes(), failing={"fallback"})

    def test_open_redirect_fails_and_is_never_followed(self):
        target = f"{self.fixture.origin}/followed"
        for fallback, overrides in ((None, {"/": (302, {"location": target}, "")}),
                                    (None, {f"/{SLUG}": (302, {"location": target}, "")}),
                                    ("twoInvite", {f"/{SLUG}": (302, {"location": "https://discord.gg/other"}, "")})):
            with self.subTest(fallback=fallback, overrides=overrides):
                self.server.fallback = fallback
                self.server.overrides = overrides
                self.assertVerdicts(self.run_probes(), failing={"fallback"})
        self.assertNotIn("/followed", [r["path"] for r in self.server.seen])

    def test_reserved_alias_leaks_fail(self):
        for path, reply in (("/healthz/", (302, {"location": "https://discord.gg/x"}, "")),
                            ("/%68ealthz", (200, {}, "ok\n")),
                            ("//healthz", (404, {"location": "https://discord.gg/x"}, "")),
                            ("/healthz", (200, {}, "campaign page")),
                            ("/healthz", (302, {"location": "https://discord.gg/x"}, ""))):
            with self.subTest(path=path):
                self.server.overrides = {path: reply}
                self.assertVerdicts(self.run_probes(), failing={"reserved_healthz"})

    def test_ops_metrics_must_not_answer_without_a_bearer(self):
        self.server.overrides = {"/ops/metrics": (401, {"www-authenticate": "Bearer"}, "unauthorized")}
        self.assertVerdicts(self.run_probes())
        for overrides in ({"/ops/metrics": (200, {}, "")},
                          {"/ops/metrics": (404, {}, "# TYPE x counter\nx 1\n")},
                          {"/metrics": (200, {}, "# HELP x\n")},
                          {"/%6detrics": (302, {"location": "https://discord.gg/x"}, "")}):
            with self.subTest(overrides=overrides):
                self.server.overrides = overrides
                self.assertVerdicts(self.run_probes(), failing={"ops_metrics"})

    def test_internal_error_text_fails_without_echoing_it(self):
        leak = "TypeError: boom at handler (index.ts:12) postgres://u:hunter2@db/x"
        self.server.overrides = {"/ops/metrics": (404, {}, leak), "/health": (500, {}, "error code: 1101")}
        results = self.run_probes()
        self.assertVerdicts(results, failing={"health", "no_internal_error_text"})
        failures = results["no_internal_error_text"]["failures"]
        self.assertTrue(any("js_error" in f for f in failures))
        self.assertTrue(any("connection_string" in f for f in failures))
        self.assertTrue(any("worker_exception" in f for f in failures))
        self.assertNotIn("hunter2", json.dumps(results))

    def test_unreachable_target_fails_cleanly(self):
        fetch = make_fetch("http://127.0.0.1:9", timeout=2)
        results = {r["name"]: r for r in run_probes(fetch, unknown_slug=SLUG)}
        for name in ("health", "readyz", "reserved_healthz", "fallback", "ops_metrics"):
            self.assertEqual(results[name]["verdict"], "FAIL", name)
        self.assertEqual(results["health"]["requests"][0]["status"], None)

    def test_main_writes_the_json_report(self):
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        report_path = Path(scratch.name) / "report.json"
        origin = self.fixture.origin
        with mock.patch.object(staging_probe, "fence", return_value=origin), \
                contextlib.redirect_stderr(io.StringIO()):
            code = staging_probe.main(["--base-url", STAGING, "--worker-version", VERSION,
                                       "--report", str(report_path)])
        self.assertEqual(code, 0)
        report = json.loads(report_path.read_text())
        self.assertEqual(report["verdict"], "PASS")
        self.assertEqual(report["worker_version"], VERSION)
        self.assertEqual(report["target"], origin)
        self.assertRegex(report["window_utc"]["start"], r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ$")
        self.assertLessEqual(report["window_utc"]["start"], report["window_utc"]["end"])
        self.assertEqual([p["name"] for p in report["probes"]],
                         ["health", "readyz", "reserved_healthz", "fallback", "ops_metrics",
                          "no_internal_error_text"])

        self.server.readyz = (200, PARKED)
        with mock.patch.object(staging_probe, "fence", return_value=origin), \
                contextlib.redirect_stderr(io.StringIO()), contextlib.redirect_stdout(io.StringIO()) as out:
            self.assertEqual(staging_probe.main(["--base-url", STAGING, "--worker-version", VERSION]), 1)
        self.assertEqual(json.loads(out.getvalue())["verdict"], "FAIL")


if __name__ == "__main__":
    unittest.main()
