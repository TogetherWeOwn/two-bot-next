"""Offline redirect-smoke fixtures; stdlib only, no network access."""

import contextlib
import http.client
import io
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import staging_redirect_smoke as smoke  # noqa: E402

STAGING = "https://two-bot-next-staging.example-sub.workers.dev"
SLUG = "staging-smoke-unknown-slug"
SENTINEL = "RAW_SECRET_SENTINEL_never_disclose_database_password"

HEALTHZ = (200, {"content-type": "text/plain"}, b"ok\n")
# The deployed Worker-level guard answers `not found` (no trailing newline);
# redirect.ts answers `not found\n`. Both are the documented 404-with-no-lookup.
RESERVED = (404, {"content-type": "text/plain;charset=UTF-8"}, b"not found")
RESERVED_REDIRECT_LAYER = (404, {"content-type": "text/plain"}, b"not found\n")
UNKNOWN_404 = (404, {"content-type": "text/plain"}, b"not found\n")
UNKNOWN_FALLBACK = (302, {"location": "https://discord.gg/fallbackCode",
                           "cache-control": "no-store, no-cache, must-revalidate",
                           "referrer-policy": "no-referrer"}, b"redirecting\n")
MISCONFIGURED = (503, {"retry-after": "30"}, b"redirect service misconfigured\n")
UNAVAILABLE = (503, {"retry-after": "30"}, b"temporarily unavailable\n")
BAD_CAMPAIGN = (500, {"content-type": "text/plain"}, b"misconfigured campaign\n")


class RedirectSmokeTests(unittest.TestCase):
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

    def fetch(self, responses):
        routes = dict(responses)

        def fake(url):
            self.requested.append(url)
            if url not in routes:
                raise AssertionError(f"unexpected offline fixture route: {url}")
            response = routes[url]
            if isinstance(response, Exception):
                raise response
            status, headers, body = response
            return status, dict(headers), body
        return fake

    def argv(self, *extra):
        return ["--staging-url", STAGING, "--unknown-slug", SLUG,
                "--evidence", self.evidence, *extra]

    def run_smoke(self, responses, *extra):
        args = smoke.parse_args(self.argv(*extra))
        with mock.patch.dict(os.environ, {}, clear=True):
            origin, results = smoke.run(args, self.fetch(responses))
        return origin, {result.name: result for result in results}

    def all_pass(self, responses):
        origin, results = self.run_smoke(responses)
        self.assertEqual(origin, STAGING)
        self.assertEqual({name for name, result in results.items() if result.ok},
                         {"healthz", "reserved", "unknown"})
        return results

    def assert_only_failure(self, name, reason, results):
        self.assertEqual({n for n, r in results.items() if r.ok},
                         {"healthz", "reserved", "unknown"} - {name})
        self.assertIn(reason, results[name].reason)

    def test_all_pass_with_unknown_404(self):
        results = self.all_pass({STAGING + "/healthz": HEALTHZ,
                                 STAGING + "/HEALTHZ": RESERVED,
                                 STAGING + "/" + SLUG: UNKNOWN_404})
        self.assertIn("exact probe shape", results["healthz"].reason)
        self.assertIn("no redirect target", results["reserved"].reason)
        self.assertIn("404 with no redirect target", results["unknown"].reason)
        self.assertEqual(self.requested, [STAGING + "/healthz", STAGING + "/HEALTHZ",
                                          STAGING + "/" + SLUG])

    def test_all_pass_with_configured_fallback(self):
        results = self.all_pass({STAGING + "/healthz": HEALTHZ,
                                 STAGING + "/HEALTHZ": RESERVED,
                                 STAGING + "/" + SLUG: UNKNOWN_FALLBACK})
        self.assertIn("configured fallback", results["unknown"].reason)

    def test_reserved_accepts_either_documented_404_shape(self):
        for reserved in [RESERVED, RESERVED_REDIRECT_LAYER]:
            with self.subTest(body=reserved[2]):
                results = self.all_pass({STAGING + "/healthz": HEALTHZ,
                                         STAGING + "/HEALTHZ": reserved,
                                         STAGING + "/" + SLUG: UNKNOWN_404})
                self.assertIn("no redirect target", results["reserved"].reason)

    def test_any_5xx_must_be_a_bounded_fail_closed_shape(self):
        for status, headers, body, shape in [
                (503, {"retry-after": "30"}, b"redirect service misconfigured\n",
                 "503 redirect service misconfigured"),
                (503, {"retry-after": "30", "location": None}, b"temporarily unavailable\n",
                 "503 temporarily unavailable"),
                (500, {}, b"misconfigured campaign\n", "500 misconfigured campaign")]:
            with self.subTest(status=status, body=body):
                origin, results = self.run_smoke({STAGING + "/healthz": HEALTHZ,
                                                  STAGING + "/HEALTHZ": RESERVED,
                                                  STAGING + "/" + SLUG: (status, headers, body)})
                self.assertEqual(origin, STAGING)
                self.assertTrue(results["unknown"].ok)
                self.assertIn(shape, results["unknown"].reason)
        # Each probe classifies a fail-closed 503 the same way.
        origin, results = self.run_smoke({STAGING + "/healthz": MISCONFIGURED,
                                          STAGING + "/HEALTHZ": UNAVAILABLE,
                                          STAGING + "/" + SLUG: BAD_CAMPAIGN})
        self.assertEqual(origin, STAGING)
        for name, shape in [("healthz", "503 redirect service misconfigured"),
                            ("reserved", "503 temporarily unavailable"),
                            ("unknown", "500 misconfigured campaign")]:
            with self.subTest(name=name):
                self.assertTrue(results[name].ok)
                self.assertIn(shape, results[name].reason)

    def test_wrong_status_or_shape_fails_only_that_check(self):
        cases = {
            "healthz 404 instead of the exact probe": (
                {STAGING + "/healthz": UNKNOWN_404}, "healthz", "expected the exact 200 probe"),
            "healthz 200 with a redirect target": (
                {STAGING + "/healthz": (200, {"location": "https://discord.gg/x"}, b"ok\n")},
                "healthz", "exact probe shape"),
            "healthz 200 with the wrong body": (
                {STAGING + "/healthz": (200, {}, b"ready\n")}, "healthz", "exact probe shape"),
            "reserved alias redirecting": (
                {STAGING + "/HEALTHZ": UNKNOWN_FALLBACK}, "reserved",
                "expected 404 with no lookup"),
            "reserved alias 404 carrying a redirect target": (
                {STAGING + "/HEALTHZ": (404, {"location": "https://discord.gg/x"},
                                        b"not found")}, "reserved", "redirect target"),
            "unknown slug redirecting off host": (
                {STAGING + "/" + SLUG: (302, {"location": "https://evil.test/x",
                                              "cache-control": "no-store"}, b"y\n")},
                "unknown", "fixed redirect host"),
            "unknown slug fallback without no-store": (
                {STAGING + "/" + SLUG: (302, {"location": "https://discord.gg/code"}, b"y\n")},
                "unknown", "no-store"),
            "unknown slug fallback with an invalid code": (
                {STAGING + "/" + SLUG: (302, {"location": "https://discord.gg/has space",
                                              "cache-control": "no-store"}, b"y\n")},
                "unknown", "invalid invite code"),
            "unknown slug 405 instead of fallback-or-404": (
                {STAGING + "/" + SLUG: (405, {"allow": "GET, HEAD"}, b"")},
                "unknown", "expected fallback-or-404"),
            "5xx with a redirect target is never fail-closed": (
                {STAGING + "/" + SLUG: (503, {"retry-after": "30",
                                              "location": "https://discord.gg/x"},
                                        b"redirect service misconfigured\n")},
                "unknown", "with a redirect target"),
            "5xx with an unknown body is never fail-closed": (
                {STAGING + "/" + SLUG: (503, {"retry-after": "30"}, b"oops\n")},
                "unknown", "bounded fail-closed shapes"),
            "503 without retry-after is never fail-closed": (
                {STAGING + "/" + SLUG: (503, {}, b"redirect service misconfigured\n")},
                "unknown", "bounded fail-closed shapes"),
        }
        for label, (routes, name, reason) in cases.items():
            with self.subTest(label):
                self.requested.clear()
                full = {STAGING + "/healthz": HEALTHZ, STAGING + "/HEALTHZ": RESERVED,
                        STAGING + "/" + SLUG: UNKNOWN_404}
                full.update(routes)
                _, results = self.run_smoke(full)
                self.assert_only_failure(name, reason, results)
                self.assertNotIn(SENTINEL, json.dumps(
                    {n: r.reason for n, r in results.items()}))

    def test_transport_errors_fail_only_that_check(self):
        for name, path in [("healthz", "/healthz"), ("reserved", "/HEALTHZ"),
                           ("unknown", "/" + SLUG)]:
            with self.subTest(name=name):
                self.requested.clear()
                routes = {STAGING + "/healthz": HEALTHZ, STAGING + "/HEALTHZ": RESERVED,
                          STAGING + "/" + SLUG: UNKNOWN_404}
                routes[STAGING + path] = TimeoutError("timed out")
                _, results = self.run_smoke(routes)
                self.assert_only_failure(name, "did not respond (TimeoutError)", results)

    def test_refuses_every_non_staging_origin_without_a_request(self):
        for url in ("https://two-bot-next-production.example-sub.workers.dev",
                    "https://two-bot-next.example-sub.workers.dev",
                    "https://go.two.gg",
                    "http://two-bot-next-staging.example-sub.workers.dev",
                    "https://two-bot-next-staging.example-sub.workers.dev:8443",
                    "https://user@two-bot-next-staging.example-sub.workers.dev",
                    "https://two-bot-next-staging.example-sub.workers.dev.evil.test",
                    "https://two-bot-next-staging.example-sub.workers.dev/healthz",
                    "https://two-bot-next-staging.example-sub.workers.dev/?x=1",
                    "https://[::1"):
            with self.subTest(url=url):
                self.requested.clear()
                args = smoke.parse_args(["--staging-url", url, "--evidence", self.evidence])
                origin, results = smoke.run(args, self.fetch({}))
                self.assertIsNone(origin)
                self.assertFalse(results[0].ok)
                self.assertIn("two-bot-next-staging", results[0].reason)
                self.assertEqual(self.requested, [])

    def test_missing_staging_url_fails_closed_without_a_request(self):
        args = smoke.parse_args(["--staging-url", "", "--evidence", self.evidence])
        origin, results = smoke.run(args, self.fetch({}))
        self.assertIsNone(origin)
        self.assertIn("no staging Worker URL given", results[0].reason)
        self.assertEqual(self.requested, [])

    def test_unknown_slug_probe_must_be_valid_unreserved_shape(self):
        for slug, reason in [("healthz", "must not be a reserved slug"),
                             ("metrics", "must not be a reserved slug"),
                             ("a", "not a valid campaign shape"),
                             ("a" * 41, "not a valid campaign shape"),
                             ("has space", "not a valid campaign shape"),
                             ("has/slash", "not a valid campaign shape"),
                             ("UPPER", "not a valid campaign shape"),
                             ("", "not a valid campaign shape")]:
            with self.subTest(slug=slug):
                self.requested.clear()
                args = smoke.parse_args(["--staging-url", STAGING, "--unknown-slug", slug,
                                         "--evidence", self.evidence])
                origin, results = smoke.run(args, self.fetch({}))
                self.assertIsNone(origin)
                self.assertIn(reason, results[0].reason)
                self.assertEqual(self.requested, [])

    def test_main_writes_allowlisted_evidence_only_on_all_pass(self):
        out = io.StringIO()
        routes = {STAGING + "/healthz": HEALTHZ, STAGING + "/HEALTHZ": RESERVED,
                  STAGING + "/" + SLUG: UNKNOWN_FALLBACK}
        with contextlib.redirect_stdout(out), mock.patch.dict(os.environ, {}, clear=True):
            code = smoke.main(self.argv(), fetch_fn=self.fetch(routes))
        self.assertEqual(code, 0)
        self.assertEqual(out.getvalue().splitlines(), [
            "PASS healthz: healthz 200 with the exact probe shape",
            "PASS reserved: reserved alias 404 with no redirect target",
            "PASS unknown: unknown slug follows the configured fallback to the fixed host",
            "staging redirect smoke: 3/3 checks passed"])
        receipt = json.loads(Path(self.evidence).read_text())
        self.assertEqual(receipt["origin"], STAGING)
        self.assertEqual(receipt["unknown_slug"], SLUG)
        self.assertEqual(receipt["result"], "pass")
        self.assertEqual([(check["check"], check["verdict"]) for check in receipt["checks"]],
                         [("healthz", "pass"), ("reserved", "pass"), ("unknown", "pass")])
        self.assertNotIn(SENTINEL, out.getvalue() + Path(self.evidence).read_text())

    def test_main_writes_no_evidence_on_failure(self):
        routes = {STAGING + "/healthz": (500, {}, b"error code: 1101"),
                  STAGING + "/HEALTHZ": RESERVED, STAGING + "/" + SLUG: UNKNOWN_404}
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = smoke.main(self.argv(), fetch_fn=self.fetch(routes))
        self.assertEqual(code, 1)
        self.assertIn("FAIL healthz", out.getvalue())
        self.assertIn("staging redirect smoke: 2/3 checks passed", out.getvalue())
        self.assertFalse(Path(self.evidence).exists())

    def test_environment_defaults_are_read(self):
        out = io.StringIO()
        routes = {STAGING + "/healthz": HEALTHZ, STAGING + "/HEALTHZ": RESERVED,
                  STAGING + "/" + SLUG: UNKNOWN_404}
        env = {"STAGING_WORKER_URL": STAGING + "/"}
        with contextlib.redirect_stdout(out), mock.patch.dict(os.environ, env, clear=True):
            code = smoke.main(["--evidence", self.evidence], fetch_fn=self.fetch(routes))
        self.assertEqual(code, 0, out.getvalue())

    def test_default_fetch_never_follows_redirects(self):
        handler = smoke._NoRedirect()
        self.assertIsNone(handler.redirect_request(None, None, 302, "Found", {},
                                                   "https://elsewhere"))

    def test_fetch_identifies_smoke_traffic_without_browser_spoofing(self):
        # The edge refuses the stdlib default `Python-urllib/*` client (error
        # 1010) before the Worker is reached; curl on the same probes passes.
        self.assertNotIn("Python-urllib", smoke.USER_AGENT)
        self.assertNotIn("Mozilla", smoke.USER_AGENT)
        seen = {}

        class FakeResponse:
            status = 200
            headers = {}
            def read(self, _cap):
                return b"ok\n"
            def __enter__(self):
                return self
            def __exit__(self, *args):
                return False

        class FakeOpener:
            def open(self, request, timeout=None):
                seen["ua"] = request.get_header("User-agent")
                seen["timeout"] = timeout
                return FakeResponse()

        with mock.patch.object(smoke.urllib.request, "build_opener",
                               return_value=FakeOpener()):
            status, _, body = smoke.fetch(STAGING + "/healthz")
        self.assertEqual((status, body), (200, b"ok\n"))
        self.assertEqual(seen["ua"], smoke.USER_AGENT)
        self.assertEqual(seen["timeout"], smoke.TIMEOUT_SECONDS)


if __name__ == "__main__":
    unittest.main()
