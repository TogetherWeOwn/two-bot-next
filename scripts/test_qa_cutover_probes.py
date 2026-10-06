"""Cutover acceptance probe regressions: fixture doubles only, no network or databases."""

import io
import json
import sys
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import qa_cutover_probes as probe  # noqa: E402

HEALTH = (200, json.dumps({"status": "ok"}).encode())
PARKED = (503, json.dumps({
    "components": [["process", "ready"], ["gateway", "down"],
                   ["database", "down"], ["token_invalid", "ready"]],
    "jobs": {"counter": {"parked": True, "running": False}},
    "build_revision": "abc123", "build_id": "42",
}).encode())
READY = (200, json.dumps({
    "components": [["process", "ready"], ["gateway", "ready"]],
    "jobs": {"counter": {"parked": False, "running": True}},
    "build_revision": "abc123", "build_id": "42",
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

    def test_truthful_503_with_gateway_ready_stays_green(self):
        body = (503, json.dumps({
            "components": [["process", "ready"], ["gateway", "ready"],
                           ["database", "down"]],
            "jobs": {"counter": {"parked": True, "running": False}},
            "build_revision": "r", "build_id": "b",
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
            "build_revision": "r", "build_id": "b",
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
            "build_revision": "r", "build_id": "b",
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
            "build_revision": "r", "build_id": "b",
        }).encode())
        code, out = run(double({"/health": HEALTH, "/readyz": body}),
                        "--base-url", "http://h/")
        self.assertEqual(code, 1)
        self.assertIn("FAIL readiness-shape:", out)

    def test_unknown_status_vocabulary_fails(self):
        body = (503, json.dumps({
            "components": [["process", "ready"], ["gateway", "bogus"]],
            "jobs": {"counter": {"parked": True, "running": False}},
            "build_revision": "r", "build_id": "b",
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
            "build_revision": "r", "build_id": "b",
        }).encode())
        code, out = run(double({"/health": HEALTH, "/readyz": body}),
                        "--base-url", "http://h/")
        self.assertEqual(code, 1)
        self.assertIn("FAIL jobs-map:", out)


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


if __name__ == "__main__":
    unittest.main()
