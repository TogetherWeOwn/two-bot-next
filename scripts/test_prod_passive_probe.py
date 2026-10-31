"""Offline passive production probe fixtures; stdlib only, no network access."""

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
PROD_ALT = "https://two-bot-next-production.example-sub.workers.dev"


def components(*pairs):
    return [[name, status] for name, status in pairs]


def body(value, status=200):
    return status, {}, json.dumps(value).encode()


READY = {
    "components": components(("process", "ready"), ("gateway", "ready"),
                             ("database", "ready"), ("token_invalid", "ready")),
    "jobs": {},
    "build_revision": "0123456789abcdef0123456789abcdef01234567",
    "build_id": "1-1",
}


class ProdPassiveProbeTests(unittest.TestCase):
    def setUp(self):
        # Any real HTTP is a test failure: the probe only gets fake fetches here.
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

    def drive(self, routes, *extra, env=None, origin=PROD):
        out = io.StringIO()
        argv = ["--prod-url", origin, "--evidence", self.evidence, *extra]
        with mock.patch.dict(os.environ, env or {}, clear=True), \
                redirect_stdout(out):
            code = probe.main(argv, fetch_fn=self.fetch(routes))
        lines = out.getvalue().splitlines()
        named = {}
        for line in lines[:-1]:
            head, _, _ = line.partition(":")
            named[head] = line
        return code, named, lines[-1]

    def assert_only_failure(self, name, reason, code, results):
        self.assertEqual(code, 1)
        self.assertEqual(set(results),
                         {f"{'FAIL' if n == name else 'PASS'} {n}"
                          for n in ("health", "readyz", "build")})
        self.assertIn(reason, results[f"FAIL {name}"])

    def test_staging_origin_refuses_before_any_request(self):
        routes = {}
        code, _, _ = self.drive(
            routes, origin="https://two-bot-next-staging.example-sub.workers.dev")
        self.assertEqual(code, 2)
        self.assertEqual(self.requested, [])

    def test_unknown_prod_name_refuses_before_any_request(self):
        routes = {}
        code, _, _ = self.drive(
            routes, origin="https://two-bot-next.example-sub.workers.dev.evil.example")
        self.assertEqual(code, 2)
        self.assertEqual(self.requested, [])

    def test_http_origin_refuses_before_any_request(self):
        routes = {}
        code, _, _ = self.drive(routes, origin="http://two-bot-next.example-sub.workers.dev")
        self.assertEqual(code, 2)
        self.assertEqual(self.requested, [])

    def test_credentialed_origin_refuses_before_any_request(self):
        routes = {}
        code, _, _ = self.drive(
            routes, origin="https://user@two-bot-next.example-sub.workers.dev")
        self.assertEqual(code, 2)
        self.assertEqual(self.requested, [])

    def test_path_origin_refuses_before_any_request(self):
        routes = {}
        code, _, _ = self.drive(routes, origin=PROD + "/health")
        self.assertEqual(code, 2)
        self.assertEqual(self.requested, [])

    def test_missing_origin_refuses_with_no_request(self):
        out = io.StringIO()
        with mock.patch.dict(os.environ, {}, clear=True), redirect_stdout(out):
            code = probe.main(["--prod-url", "", "--evidence", self.evidence],
                              fetch_fn=self.fetch({}))
        self.assertEqual(code, 2)
        self.assertIn("FAIL origin", out.getvalue())
        self.assertEqual(self.requested, [])

    def test_all_ready_passes_and_records_build_identity(self):
        routes = {PROD + "/health": body({"status": "ok"}),
                  PROD + "/readyz": body(dict(READY))}
        code, results, summary = self.drive(routes)
        self.assertEqual(code, 0)
        self.assertEqual(set(results), {"PASS health", "PASS readyz", "PASS build"})
        self.assertIn("3/3 checks passed", summary)
        receipt = json.loads(Path(self.evidence).read_text())
        self.assertEqual(receipt["origin"], PROD)
        self.assertEqual(receipt["result"], "pass")
        self.assertEqual(receipt["build_revision"], "0123456789abcdef0123456789abcdef01234567")
        self.assertEqual(receipt["build_id"], "1-1")
        self.assertIsNone(receipt["gateway_failure_class"])
        self.assertIsNone(receipt["gateway_failure_root_cause"])
        self.assertEqual([(check["check"], check["verdict"]) for check in receipt["checks"]],
                         [("health", "pass"), ("readyz", "pass"), ("build", "pass")])

    def test_production_alias_passes(self):
        routes = {PROD_ALT + "/health": body({"status": "ok"}),
                  PROD_ALT + "/readyz": body(dict(READY))}
        code, results, _ = self.drive(routes, origin=PROD_ALT)
        self.assertEqual(code, 0)
        self.assertEqual(set(results), {"PASS health", "PASS readyz", "PASS build"})

    def test_parked_gateway_is_truthful_not_a_pass(self):
        degraded = dict(READY)
        degraded["components"] = components(("process", "ready"), ("gateway", "starting"),
                                            ("database", "ready"), ("token_invalid", "ready"))
        routes = {PROD + "/health": body({"status": "ok"}),
                  PROD + "/readyz": body(degraded, status=503)}
        code, results, _ = self.drive(routes)
        self.assert_only_failure("readyz", "truthful, not a pass", code, results)
        receipt = json.loads(Path(self.evidence).read_text())
        self.assertEqual(receipt["result"], "fail")
        self.assertIsNone(receipt["gateway_failure_root_cause"])

    def test_contradictory_status_fails(self):
        routes = {PROD + "/health": body({"status": "ok"}),
                  PROD + "/readyz": body(dict(READY), status=503)}
        code, results, _ = self.drive(routes)
        self.assert_only_failure("readyz", "contradicts the component breakdown", code, results)

    def test_health_shape_mismatch_fails(self):
        routes = {PROD + "/health": body({"status": "degraded"}),
                  PROD + "/readyz": body(dict(READY))}
        code, results, _ = self.drive(routes)
        self.assert_only_failure("health", "exact status shape", code, results)

    def test_unparseable_readyz_fails_breakdown_and_build(self):
        routes = {PROD + "/health": body({"status": "ok"}),
                  PROD + "/readyz": body("not json")}
        code, results, _ = self.drive(routes)
        self.assertEqual(code, 1)
        self.assertEqual(set(results), {"PASS health", "FAIL readyz", "FAIL build"})

    def test_checkpoint_read_failure_marks_cause_unverified(self):
        failed_read = dict(READY)
        failed_read["components"] = components(("process", "ready"), ("gateway", "starting"),
                                               ("database", "ready"), ("token_invalid", "ready"))
        failed_read["gateway_failure"] = {"phase": "durable_gateway",
                                          "class": "checkpoint_load_failed"}
        routes = {PROD + "/health": body({"status": "ok"}),
                  PROD + "/readyz": body(failed_read, status=503)}
        code, results, _ = self.drive(routes)
        self.assert_only_failure("readyz", "checkpoint read failed", code, results)
        receipt = json.loads(Path(self.evidence).read_text())
        self.assertEqual(receipt["gateway_failure_class"], "checkpoint_load_failed")
        self.assertEqual(receipt["gateway_failure_root_cause"], "unverified")

    def test_unknown_failure_class_refused_without_echo(self):
        bad = dict(READY)
        sentinel = "sensitive-fixture-detail"
        bad["gateway_failure"] = {"phase": "durable_gateway",
                                  "class": sentinel}
        routes = {PROD + "/health": body({"status": "ok"}),
                  PROD + "/readyz": body(bad)}
        code, results, _ = self.drive(routes)
        self.assertEqual(code, 1)
        self.assertIn("FAIL readyz", set(results))
        self.assertNotIn(sentinel, "\n".join(results.values()))
        receipt = json.loads(Path(self.evidence).read_text())
        self.assertNotIn(sentinel, json.dumps(receipt))

    def test_transport_failure_names_class_only(self):
        routes = {PROD + "/health": OSError("down"),
                  PROD + "/readyz": body(dict(READY))}
        code, results, _ = self.drive(routes)
        self.assertEqual(code, 1)
        self.assertIn("FAIL health", set(results))
        self.assertIn("OSError", results["FAIL health"])

    def test_body_over_cap_refused_without_echo(self):
        big = json.dumps({"status": "ok"}) + " " * (probe.BODY_CAP + 1)
        routes = {PROD + "/health": (200, {}, big.encode()),
                  PROD + "/readyz": body(dict(READY))}
        code, results, _ = self.drive(routes)
        self.assertEqual(code, 1)
        self.assertIn("FAIL health", set(results))
        self.assertIn("over the body cap", results["FAIL health"])

    def test_evidence_absent_on_fence_refusal(self):
        routes = {}
        code, _, _ = self.drive(routes, origin="https://example.com")
        self.assertEqual(code, 2)
        self.assertFalse(Path(self.evidence).exists())
        self.assertEqual(self.requested, [])


if __name__ == "__main__":
    unittest.main()
