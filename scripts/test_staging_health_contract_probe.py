"""Offline health-contract probe fixtures; stdlib only, no network access."""

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
import staging_health_contract_probe as probe  # noqa: E402

STAGING = "https://two-bot-next-staging.example-sub.workers.dev"


def components(*pairs):
    return [[name, status] for name, status in pairs]


def body(value, status=200):
    return status, {}, json.dumps(value).encode()


READY = {
    "components": components(("process", "ready"), ("gateway", "ready"),
                             ("database", "ready"), ("token_invalid", "ready")),
    "jobs": {},
    "build_revision": "abc123",
    "build_id": "1-1",
}


class HealthContractProbeTests(unittest.TestCase):
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

    def drive(self, routes, *extra, env=None):
        out = io.StringIO()
        argv = ["--staging-url", STAGING, "--evidence", self.evidence, *extra]
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

    def assert_readyz_unreadable(self, reason, code, results):
        # An unparseable readyz body fails both the breakdown and the build
        # identity (no body to read it from); health still answers.
        self.assertEqual(code, 1)
        self.assertEqual(set(results), {"PASS health", "FAIL readyz", "FAIL build"})
        self.assertIn(reason, results["FAIL readyz"])
        self.assertIn("no readyz body", results["FAIL build"])

    def test_production_origin_refuses_before_any_request(self):
        routes = {}
        code, _, _ = self.drive(routes, "--staging-url",
                              "https://two-bot-next.example-sub.workers.dev")
        self.assertEqual(code, 2)
        self.assertEqual(self.requested, [])

    def test_http_origin_refuses_before_any_request(self):
        routes = {}
        code, _, _ = self.drive(routes, "--staging-url",
                              "http://two-bot-next-staging.example-sub.workers.dev")
        self.assertEqual(code, 2)
        self.assertEqual(self.requested, [])

    def test_credentialed_origin_refuses_before_any_request(self):
        routes = {}
        code, _, _ = self.drive(routes, "--staging-url",
                              "https://user@two-bot-next-staging.example-sub.workers.dev")
        self.assertEqual(code, 2)
        self.assertEqual(self.requested, [])

    def test_missing_origin_refuses_with_no_request(self):
        out = io.StringIO()
        with mock.patch.dict(os.environ, {}, clear=True), redirect_stdout(out):
            code = probe.main(["--staging-url", "", "--evidence", self.evidence],
                              fetch_fn=self.fetch({}))
        self.assertEqual(code, 2)
        self.assertIn("FAIL origin", out.getvalue())
        self.assertEqual(self.requested, [])

    def test_all_ready_passes_and_records_build_identity(self):
        routes = {STAGING + "/health": body({"status": "ok"}),
                  STAGING + "/readyz": body(dict(READY))}
        code, results, summary = self.drive(routes)
        self.assertEqual(code, 0)
        self.assertEqual(set(results), {"PASS health", "PASS readyz", "PASS build"})
        self.assertIn("3/3 checks passed", summary)
        receipt = json.loads(Path(self.evidence).read_text())
        self.assertEqual(receipt["origin"], STAGING)
        self.assertEqual(receipt["result"], "pass")
        self.assertEqual(receipt["build_revision"], "abc123")
        self.assertEqual(receipt["build_id"], "1-1")
        self.assertIsNone(receipt["gateway_failure_class"])
        self.assertEqual([(check["check"], check["verdict"]) for check in receipt["checks"]],
                         [("health", "pass"), ("readyz", "pass"), ("build", "pass")])

    def test_expected_sha_match_and_mismatch(self):
        routes = {STAGING + "/health": body({"status": "ok"}),
                  STAGING + "/readyz": body(dict(READY))}
        code, _, _ = self.drive(routes, "--expected-sha", "abc123")
        self.assertEqual(code, 0)
        code, results, _ = self.drive(routes, "--expected-sha", "deadbeef")
        self.assert_only_failure("build", "does not match the expected SHA", code, results)

    def test_parked_gateway_is_truthful_not_approval(self):
        degraded = dict(READY)
        degraded["components"] = components(("process", "ready"), ("gateway", "starting"),
                                            ("database", "ready"), ("token_invalid", "ready"))
        routes = {STAGING + "/health": body({"status": "ok"}),
                  STAGING + "/readyz": body(degraded, status=503)}
        code, results, _ = self.drive(routes)
        self.assert_only_failure("readyz", "truthful, not E2E approval", code, results)
        receipt = json.loads(Path(self.evidence).read_text())
        self.assertEqual(receipt["result"], "fail")

    def test_db_behind_binary_names_the_migration_signature(self):
        lagging = dict(READY)
        lagging["components"] = components(("process", "ready"), ("gateway", "starting"),
                                           ("database", "ready"), ("token_invalid", "ready"))
        lagging["gateway_failure"] = {"phase": "durable_gateway",
                                      "class": "checkpoint_load_failed"}
        routes = {STAGING + "/health": body({"status": "ok"}),
                  STAGING + "/readyz": body(lagging, status=503)}
        code, results, _ = self.drive(routes)
        self.assert_only_failure("readyz", "db-behind-binary signature", code, results)
        receipt = json.loads(Path(self.evidence).read_text())
        self.assertEqual(receipt["gateway_failure_class"], "checkpoint_load_failed")

    def test_readyz_200_with_down_component_contradicts_the_breakdown(self):
        lying = dict(READY)
        lying["components"] = components(("process", "ready"), ("gateway", "down"),
                                         ("database", "ready"), ("token_invalid", "ready"))
        routes = {STAGING + "/health": body({"status": "ok"}),
                  STAGING + "/readyz": body(lying, status=200)}
        code, results, _ = self.drive(routes)
        self.assert_only_failure("readyz", "contradicts the component breakdown", code, results)

    def test_ownership_refusal_is_not_the_bot_breakdown(self):
        routes = {STAGING + "/health": body({"status": "ok"}),
                  STAGING + "/readyz": body({"error": "fenced"}, status=503)}
        code, results, _ = self.drive(routes)
        self.assert_readyz_unreadable("ownership refusal or broken deploy", code, results)

    def test_missing_component_is_not_the_bot_breakdown(self):
        short = dict(READY)
        short["components"] = components(("process", "ready"), ("gateway", "ready"))
        routes = {STAGING + "/health": body({"status": "ok"}),
                  STAGING + "/readyz": body(short)}
        code, results, _ = self.drive(routes)
        self.assert_readyz_unreadable("missing database", code, results)

    def test_health_503_is_not_liveness(self):
        routes = {STAGING + "/health": body({"status": "ok"}, status=503),
                  STAGING + "/readyz": body(dict(READY))}
        code, results, _ = self.drive(routes)
        self.assert_only_failure("health", "expected the exact 200 liveness", code, results)

    def test_non_json_readyz_is_not_the_bot_breakdown(self):
        routes = {STAGING + "/health": body({"status": "ok"}),
                  STAGING + "/readyz": (503, {}, b"bad gateway\n")}
        code, results, _ = self.drive(routes)
        self.assert_readyz_unreadable("without the bot's component breakdown", code, results)

    def test_unreachable_probe_reports_the_transport_class(self):
        routes = {STAGING + "/health": OSError("down"),
                  STAGING + "/readyz": body(dict(READY))}
        code, results, _ = self.drive(routes)
        self.assert_only_failure("health", "did not respond (OSError)", code, results)

    def test_redirect_target_is_observed_not_followed(self):
        routes = {STAGING + "/health": body({"status": "ok"}),
                  STAGING + "/readyz": probe.urllib.error.HTTPError(
                      STAGING + "/readyz", 302, "Found", {"Location": "https://elsewhere.test/"}, None)}
        code, results, _ = self.drive(routes)
        self.assertEqual(code, 1)
        self.assertIn("FAIL readyz", set(results))
        self.assertEqual(self.requested, [STAGING + "/health", STAGING + "/readyz"])

    def dup_value(self, pairs, status=200):
        value = dict(READY)
        value["components"] = components(*pairs)
        return status, value

    def test_duplicate_down_then_ready_rejected_before_verdict(self):
        _, value = self.dup_value((("process", "ready"), ("gateway", "down"),
                                   ("database", "ready"), ("token_invalid", "ready"),
                                   ("gateway", "ready")))
        with self.assertRaisesRegex(probe.ProbeError, "repeats a component name"):
            probe.parse_readyz(200, value)

    def test_duplicate_ready_then_down_rejected_before_verdict(self):
        _, value = self.dup_value((("process", "ready"), ("gateway", "ready"),
                                   ("database", "ready"), ("token_invalid", "ready"),
                                   ("gateway", "down")), status=503)
        with self.assertRaisesRegex(probe.ProbeError, "repeats a component name"):
            probe.parse_readyz(503, value)

    def test_duplicate_same_value_rejected(self):
        _, value = self.dup_value((("process", "ready"), ("gateway", "ready"),
                                   ("database", "ready"), ("token_invalid", "ready"),
                                   ("process", "ready")))
        with self.assertRaisesRegex(probe.ProbeError, "repeats a component name"):
            probe.parse_readyz(200, value)

    def test_distinct_components_still_parse(self):
        _, value = self.dup_value((("process", "ready"), ("gateway", "ready"),
                                   ("database", "ready"), ("token_invalid", "ready")))
        state, _ = probe.parse_readyz(200, value)
        self.assertEqual(state["gateway"], "ready")

    def test_duplicate_diagnostic_echoes_no_component_data(self):
        sentinel = "SENTINEL_dup_health_abc123"
        _, value = self.dup_value((("process", "ready"), ("gateway", "ready"),
                                   ("database", "ready"), ("token_invalid", "ready"),
                                   (sentinel, "ready"), (sentinel, "down")), status=503)
        with self.assertRaises(probe.ProbeError) as ctx:
            probe.parse_readyz(503, value)
        self.assertIn("repeats a component name", str(ctx.exception))
        self.assertNotIn(sentinel, str(ctx.exception))

    def test_duplicate_readyz_fails_closed_end_to_end(self):
        dup = dict(READY)
        dup["components"] = components(("process", "ready"), ("gateway", "down"),
                                       ("database", "ready"), ("token_invalid", "ready"),
                                       ("gateway", "ready"))
        routes = {STAGING + "/health": body({"status": "ok"}),
                  STAGING + "/readyz": body(dup, status=200)}
        code, results, _ = self.drive(routes)
        self.assert_readyz_unreadable("repeats a component name", code, results)


if __name__ == "__main__":
    unittest.main()
