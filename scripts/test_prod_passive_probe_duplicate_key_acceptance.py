"""Protected acceptance: prod passive probe must refuse duplicate JSON keys.

Offline only (stdlib, fake fetches, no network). No implementation source
or diff is used to derive expectations. The combined fix must retain this
file byte-identically; the builder must make these tests pass without
editing it.

Contract (baseline 8978f1249): bare json.loads last-wins before verdict
grading. Fixed code uses object_pairs_hook=_reject_duplicate_keys
(precedent: rollback_readiness_probe, qa_cutover_probes) and refuses with
fixed vocabulary containing "duplicate", echoing no raw key, value, body
or header. Refused /health fails only health; refused /readyz fails
readyz and build with no identity retained. Valid bodies stay
byte-identical.
"""

import importlib
import io
import json
import os
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
probe = importlib.import_module("prod_passive_probe")

PROD = "https://two-bot-next.example-sub.workers.dev"
SHA = "0123456789abcdef0123456789abcdef01234567"
OTHER_SHA = "fedcba9876543210fedcba9876543210fedcba98"
BUILD_ID = "1-1"
SENTINEL = "SENTINEL_DUP_PROD_7c1e4a9b"
HEADER_SENTINEL = "SENTINEL_DUP_HEADER_8d3f2a6e"


def components(*pairs):
    return [[name, status] for name, status in pairs]


def ready_report(revision=SHA, build_id=BUILD_ID):
    return {
        "components": components(("process", "ready"), ("gateway", "ready"),
                                 ("database", "ready"), ("token_invalid", "ready")),
        "jobs": {},
        "build_revision": revision,
        "build_id": build_id,
    }


def body(value, status=200, headers=None):
    return status, dict(headers or {}), json.dumps(value).encode()


def raw_body(raw: bytes, status=200, headers=None):
    return status, dict(headers or {}), raw


class ProdDuplicateKeyAcceptanceTests(unittest.TestCase):
    def setUp(self):
        guard = mock.patch.object(probe.urllib.request.OpenerDirector, "open",
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

    def drive(self, health_route, readyz_route, origin=PROD):
        routes = {origin + "/health": health_route,
                  origin + "/readyz": readyz_route}
        out = io.StringIO()
        argv = ["--prod-url", origin, "--evidence", self.evidence]
        with mock.patch.dict(os.environ, {}, clear=True), redirect_stdout(out):
            code = probe.main(argv, fetch_fn=self.fetch(routes))
        lines = out.getvalue().splitlines()
        named = {}
        for line in lines[:-1]:
            head, _, _ = line.partition(":")
            named[head] = line
        receipt = None
        receipt_text = None
        if Path(self.evidence).exists():
            receipt_text = Path(self.evidence).read_text()
            receipt = json.loads(receipt_text)
        return code, named, lines[-1], out.getvalue(), receipt_text, receipt

    def assert_no_raw_echo(self, *texts):
        for text in texts:
            if text is None:
                continue
            self.assertNotIn(SENTINEL, text)
            self.assertNotIn(HEADER_SENTINEL, text)
            self.assertNotIn("SENTINEL_DUP", text)

    # -- controls ------------------------------------------------------------

    # Compare every receipt byte with the observation clock held constant.
    @mock.patch.object(probe.time, "strftime", return_value="2026-10-24T03:11:40Z")
    def test_valid_bodies_pass_byte_identical(self, _clock):
        first = self.drive(body({"status": "ok"}), body(ready_report()))
        code, results, summary, out, receipt_text, receipt = first
        self.assertEqual(code, 0)
        self.assertEqual(set(results), {"PASS health", "PASS readyz", "PASS build"})
        self.assertIn("3/3 checks passed", summary)
        self.assertEqual(receipt["build_revision"], SHA)
        Path(self.evidence).unlink()
        self.requested = []
        second = self.drive(body({"status": "ok"}), body(ready_report()))
        self.assertEqual(second[0], 0)
        self.assertEqual(second[3], out)
        self.assertEqual(second[4], receipt_text)

    def test_receipts_keep_distinct_observation_seconds(self):
        with mock.patch.object(probe.time, "strftime", side_effect=[
                "2026-10-24T03:11:40Z", "2026-10-24T03:11:41Z"]):
            first = self.drive(body({"status": "ok"}), body(ready_report()))
            Path(self.evidence).unlink()
            self.requested = []
            second = self.drive(body({"status": "ok"}), body(ready_report()))
        self.assertEqual(first[0], 0)
        self.assertEqual(second[0], 0)
        self.assertEqual(first[5]["utc"], "2026-10-24T03:11:40Z")
        self.assertEqual(second[5]["utc"], "2026-10-24T03:11:41Z")
        self.assertEqual(first[3], second[3])
        self.assertNotEqual(first[4], second[4])

    # -- /health duplicates ---------------------------------------------------

    def assert_health_refused(self, health_raw: bytes):
        headers = {"X-Dup-Sentinel": HEADER_SENTINEL}
        code, results, _, out, receipt_text, receipt = self.drive(
            raw_body(health_raw, headers=headers), body(ready_report()))
        self.assertEqual(code, 1, f"duplicate /health must not pass: {results}")
        self.assertEqual(set(results), {"FAIL health", "PASS readyz", "PASS build"})
        self.assertIn("duplicate", results["FAIL health"].lower())
        self.assert_no_raw_echo(out, receipt_text, results["FAIL health"])
        self.assertEqual(receipt["result"], "fail")
        self.assertEqual(receipt["build_revision"], SHA)

    def test_health_conflicting_both_orders_refused(self):
        for raw in (b'{"status": "ok", "status": "degraded"}',
                    b'{"status": "degraded", "status": "ok"}'):
            with self.subTest(raw=raw):
                Path(self.evidence).unlink(missing_ok=True)
                self.requested = []
                self.assert_health_refused(raw)

    def test_health_overwritten_sentinel_refused_without_echo(self):
        raw = b'{"status": "' + SENTINEL.encode() + b'", "status": "ok"}'
        self.assert_health_refused(raw)

    def test_health_identical_values_refused(self):
        self.assert_health_refused(b'{"status": "ok", "status": "ok"}')

    def test_health_nested_duplicate_refused(self):
        raw = (b'{"status": "ok", "extra": {"a": 1, "a": 2}, '
               b'"note": "' + SENTINEL.encode() + b'"}')
        self.assert_health_refused(raw)

    # -- /readyz duplicates ---------------------------------------------------

    def assert_readyz_refused(self, readyz_raw: bytes, status=200):
        headers = {"X-Dup-Sentinel": HEADER_SENTINEL}
        code, results, _, out, receipt_text, receipt = self.drive(
            body({"status": "ok"}), raw_body(readyz_raw, status=status, headers=headers))
        self.assertEqual(code, 1, f"duplicate /readyz must not pass: {results}")
        self.assertEqual(set(results), {"PASS health", "FAIL readyz", "FAIL build"})
        self.assertIn("duplicate", results["FAIL readyz"].lower())
        self.assert_no_raw_echo(out, receipt_text,
                                results["FAIL readyz"], results["FAIL build"])
        self.assertEqual(receipt["result"], "fail")
        self.assertIsNone(receipt.get("build_revision"))
        self.assertIsNone(receipt.get("build_id"))
        self.assertIsNone(receipt.get("gateway_failure_class"))

    def test_readyz_build_revision_conflicting_both_orders_refused(self):
        base = ready_report()
        for first, second in ((SHA, OTHER_SHA), (OTHER_SHA, SHA)):
            with self.subTest(first=first[:8], second=second[:8]):
                Path(self.evidence).unlink(missing_ok=True)
                self.requested = []
                raw = (json.dumps({"components": base["components"],
                                            "jobs": base["jobs"]})[:-1].encode()
                       + (' , "build_revision": "%s", "build_revision": "%s",'
                          ' "build_id": "%s"}' % (first, second, BUILD_ID)).encode())
                self.assert_readyz_refused(raw)

    def test_readyz_overwritten_sentinel_refused_without_echo(self):
        base = ready_report()
        raw = (json.dumps({"components": base["components"],
                                    "jobs": base["jobs"]})[:-1].encode()
               + (' , "build_revision": "%s", "build_revision": "%s",'
                  ' "build_id": "%s"}' % (SENTINEL, SHA, BUILD_ID)).encode())
        self.assertEqual(json.loads(raw)["build_revision"], SHA)
        self.assert_readyz_refused(raw)

    def test_readyz_identical_values_refused(self):
        base = ready_report()
        raw = (json.dumps({"components": base["components"],
                                    "jobs": base["jobs"]})[:-1].encode()
               + (' , "build_revision": "%s", "build_revision": "%s",'
                  ' "build_id": "%s"}' % (SHA, SHA, BUILD_ID)).encode())
        self.assert_readyz_refused(raw)

    def test_readyz_nested_gateway_failure_duplicate_refused(self):
        base = ready_report()
        nested = ('"gateway_failure": {"phase": "durable_gateway", '
                  '"class": "checkpoint_load_failed", '
                  '"class": "gateway_runtime_failed", '
                  '"note": "%s"}' % SENTINEL).encode()
        raw = (json.dumps({"components": base["components"], "jobs": base["jobs"],
                                    "build_revision": SHA, "build_id": BUILD_ID})[:-1].encode()
               + b", " + nested + b"}")
        self.assert_readyz_refused(raw, status=503)

    def test_readyz_nested_jobs_duplicate_refused(self):
        base = ready_report()
        nested = ('"jobs": {"a": 1, "a": 2}').encode()
        raw = (json.dumps({"components": base["components"],
                                    "build_revision": SHA, "build_id": BUILD_ID})[:-1].encode()
               + b", " + nested + b"}")
        self.assert_readyz_refused(raw)


if __name__ == "__main__":
    unittest.main()
