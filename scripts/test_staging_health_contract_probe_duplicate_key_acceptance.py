"""Protected acceptance: drill/probe JSON must refuse duplicate object keys.

Offline only (stdlib, fake fetches, no network). No implementation source
or diff is used to derive expectations. The combined fix must retain this
file byte-identically; the builder must make these tests pass without
editing it.

Contract for staging_health_contract_probe (baseline 8978f1249):
- Bare json.loads last-wins lets a repeated key grade from the surviving
  value. Fixed code must refuse any duplicate object key at any depth via
  object_pairs_hook=_reject_duplicate_keys (precedent:
  rollback_readiness_probe.decode_strict, qa_cutover_probes).
- Refusal is fixed vocabulary containing the shape token "duplicate" and
  naming no raw key, value, body bytes or header bytes.
- A refused /health body fails only the health check; a refused /readyz
  body fails readyz and build (no report to grade, no identity retained).
  Valid bodies keep byte-identical verdicts.
"""

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
import staging_health_contract_probe as probe

STAGING = "https://two-bot-next-staging.example-sub.workers.dev"
SHA = "0123456789abcdef0123456789abcdef01234567"
OTHER_SHA = "fedcba9876543210fedcba9876543210fedcba98"
BUILD_ID = "37512552064-1"
SENTINEL = "SENTINEL_DUP_HEALTH_9f3b1c7e"
HEADER_SENTINEL = "SENTINEL_DUP_HEADER_4a2d8f1c"


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


class DuplicateKeyAcceptanceTests(unittest.TestCase):
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

    def drive(self, health_route, readyz_route):
        routes = {STAGING + "/health": health_route,
                  STAGING + "/readyz": readyz_route}
        out = io.StringIO()
        argv = ["--staging-url", STAGING, "--evidence", self.evidence]
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

    # -- controls: valid bodies keep byte-identical verdicts -----------------

    # Compare every receipt byte with the observation clock held constant.
    @mock.patch.object(probe.time, "strftime", return_value="2026-10-24T03:11:40Z")
    def test_valid_bodies_pass_byte_identical(self, _clock):
        first = self.drive(body({"status": "ok"}), body(ready_report()))
        code, results, summary, out, receipt_text, receipt = first
        self.assertEqual(code, 0)
        self.assertEqual(set(results), {"PASS health", "PASS readyz", "PASS build"})
        self.assertIn("3/3 checks passed", summary)
        self.assertEqual(receipt["build_revision"], SHA)
        self.assertEqual(receipt["build_id"], BUILD_ID)
        # A second run with the same valid bytes must be byte-identical.
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

    def test_valid_parked_readyz_still_truthful(self):
        parked = ready_report()
        parked["components"] = components(("process", "ready"), ("gateway", "starting"),
                                          ("database", "ready"), ("token_invalid", "ready"))
        code, results, _, _, _, receipt = self.drive(body({"status": "ok"}),
                                                     body(parked, status=503))
        self.assertEqual(code, 1)
        self.assertIn("FAIL readyz", set(results))
        self.assertIn("PASS build", set(results))
        self.assertEqual(receipt["build_revision"], SHA)

    # -- /health duplicates: zero grading from the refused body --------------

    def assert_health_refused(self, health_raw: bytes):
        headers = {"X-Dup-Sentinel": HEADER_SENTINEL}
        code, results, _, out, receipt_text, receipt = self.drive(
            raw_body(health_raw, headers=headers), body(ready_report()))
        self.assertEqual(code, 1, f"duplicate /health must not pass: {results}")
        self.assertEqual(set(results), {"FAIL health", "PASS readyz", "PASS build"})
        self.assertIn("duplicate", results["FAIL health"].lower())
        self.assert_no_raw_echo(out, receipt_text, results["FAIL health"])
        self.assertIsNotNone(receipt)
        self.assertEqual(receipt["result"], "fail")
        # The readyz leg still grades from its own valid body.
        self.assertEqual(receipt["build_revision"], SHA)
        self.assertEqual(self.requested, [STAGING + "/health", STAGING + "/readyz"])

    def test_health_conflicting_ok_then_degraded_refused(self):
        # Last-wins today grades "degraded" (FAIL for the wrong reason).
        self.assert_health_refused(b'{"status": "ok", "status": "degraded"}')

    def test_health_conflicting_degraded_then_ok_refused(self):
        # Last-wins today grades "ok" (PASS from a refused body).
        self.assert_health_refused(b'{"status": "degraded", "status": "ok"}')

    def test_health_overwritten_sentinel_value_refused_without_echo(self):
        # The first value is overwritten today; it must never surface.
        raw = json.dumps({"status": SENTINEL})[:-1].encode() + b', "status": "ok"}'
        # raw is {"status": "SENTINEL...", "status": "ok"} with a duplicate key.
        self.assertIn(SENTINEL, raw.decode())
        self.assert_health_refused(raw)

    def test_health_identical_values_refused(self):
        self.assert_health_refused(b'{"status": "ok", "status": "ok"}')

    def test_health_nested_duplicate_refused(self):
        raw = (b'{"status": "ok", "extra": {"a": 1, "a": 2}, '
               b'"note": "' + SENTINEL.encode() + b'"}')
        self.assert_health_refused(raw)

    def test_health_escaped_key_duplicate_refused(self):
        # "s..." decodes to "status": a repeat after escape decoding.
        raw = b'{"status": "ok", "\\u0073\\u0074\\u0061\\u0074\\u0075\\u0073": "ok"}'
        self.assertEqual(json.loads(raw), {"status": "ok"})
        self.assert_health_refused(raw)

    # -- /readyz duplicates: no breakdown, no identity from refused bytes ----

    def assert_readyz_refused(self, readyz_raw: bytes, status=200):
        headers = {"X-Dup-Sentinel": HEADER_SENTINEL}
        code, results, _, out, receipt_text, receipt = self.drive(
            body({"status": "ok"}), raw_body(readyz_raw, status=status, headers=headers))
        self.assertEqual(code, 1, f"duplicate /readyz must not pass: {results}")
        self.assertEqual(set(results), {"PASS health", "FAIL readyz", "FAIL build"})
        self.assertIn("duplicate", results["FAIL readyz"].lower())
        self.assertIn("no readyz body", results["FAIL build"])
        self.assert_no_raw_echo(out, receipt_text,
                                results["FAIL readyz"], results["FAIL build"])
        self.assertIsNotNone(receipt)
        self.assertEqual(receipt["result"], "fail")
        self.assertIsNone(receipt.get("build_revision"))
        self.assertIsNone(receipt.get("build_id"))
        self.assertIsNone(receipt.get("gateway_failure_class"))
        self.assertIsNone(receipt.get("gateway_failure_root_cause"))

    def readyz_with(self, fragment: bytes) -> bytes:
        # Splice a duplicate fragment into an otherwise valid readyz body.
        base = ready_report()
        prefix = json.dumps({k: base[k] for k in ("components", "jobs")})[:-1].encode()
        suffix = b', "build_revision": "%s", "build_id": "%s"}' % (
            base["build_revision"].encode(), base["build_id"].encode())
        # Caller supplies the duplicate pair; we keep the valid tail.
        return prefix + b", " + fragment + suffix

    def test_readyz_build_revision_conflicting_both_orders_refused(self):
        for first, second in ((SHA, OTHER_SHA), (OTHER_SHA, SHA)):
            with self.subTest(first=first[:8], second=second[:8]):
                Path(self.evidence).unlink(missing_ok=True)
                self.requested = []
                frag = ('"build_revision": "%s", "build_revision": "%s"'
                        % (first, second)).encode()
                # Build the full body manually: components/jobs + dup + build_id.
                base = ready_report()
                raw = (json.dumps({"components": base["components"],
                                            "jobs": base["jobs"]})[:-1].encode()
                       + b", " + frag
                       + (' , "build_id": "%s"}' % BUILD_ID).encode())
                self.assert_readyz_refused(raw)

    def test_readyz_build_revision_overwritten_sentinel_refused(self):
        base = ready_report()
        raw = (json.dumps({"components": base["components"],
                                    "jobs": base["jobs"]})[:-1].encode()
               + (' , "build_revision": "%s", "build_revision": "%s", "build_id": "%s"}'
                  % (SENTINEL, SHA, BUILD_ID)).encode())
        # Sanity: today this last-wins to the valid SHA and passes build.
        self.assertEqual(json.loads(raw)["build_revision"], SHA)
        self.assert_readyz_refused(raw)

    def test_readyz_identical_build_revision_refused(self):
        base = ready_report()
        raw = (json.dumps({"components": base["components"],
                                    "jobs": base["jobs"]})[:-1].encode()
               + (' , "build_revision": "%s", "build_revision": "%s", "build_id": "%s"}'
                  % (SHA, SHA, BUILD_ID)).encode())
        self.assert_readyz_refused(raw)

    def test_readyz_nested_gateway_failure_duplicate_refused(self):
        base = ready_report()
        base["gateway_failure"] = {"phase": "durable_gateway",
                                   "class": "checkpoint_load_failed"}
        # Duplicate "class" inside the nested object, both valid values.
        nested = ('"gateway_failure": {"phase": "durable_gateway", '
                  '"class": "checkpoint_load_failed", '
                  '"class": "gateway_runtime_failed", '
                  '"note": "%s"}' % SENTINEL).encode()
        raw = (json.dumps({"components": base["components"], "jobs": base["jobs"],
                                    "build_revision": SHA, "build_id": BUILD_ID})[:-1].encode()
               + b", " + nested + b"}")
        self.assertEqual(json.loads(raw)["gateway_failure"]["class"],
                         "gateway_runtime_failed")
        self.assert_readyz_refused(raw, status=503)

    def test_readyz_nested_jobs_duplicate_refused(self):
        base = ready_report()
        nested = ('"jobs": {"a": 1, "a": 2, "note": "%s"}' % SENTINEL).encode()
        raw = (json.dumps({"components": base["components"],
                                    "build_revision": SHA, "build_id": BUILD_ID})[:-1].encode()
               + b", " + nested + b"}")
        self.assert_readyz_refused(raw)

    def test_readyz_top_level_components_duplicate_refused(self):
        # Two valid components arrays: last-wins passes today.
        comps = json.dumps(ready_report()["components"]).encode()
        raw = (b'{"components": ' + comps + b', "components": ' + comps
               + b', "jobs": {}, "build_revision": "%s", "build_id": "%s"}'
               % (SHA.encode(), BUILD_ID.encode()))
        self.assert_readyz_refused(raw)

    # -- seam: the JSON decode itself must refuse -----------------------------

    def test_get_json_refuses_duplicate_health_bytes(self):
        routes = {STAGING + "/x": raw_body(b'{"status": "ok", "status": "ok"}')}
        with self.assertRaises(probe.ProbeError) as ctx:
            probe.get_json(self.fetch(routes), STAGING + "/x", "x")
        self.assertIn("duplicate", str(ctx.exception).lower())
        self.assertNotIn(SENTINEL, str(ctx.exception))

    def test_get_json_refuses_nested_duplicate_bytes(self):
        raw = b'{"a": {"b": 1, "b": 2}}'
        routes = {STAGING + "/x": raw_body(raw)}
        with self.assertRaises(probe.ProbeError) as ctx:
            probe.get_json(self.fetch(routes), STAGING + "/x", "x")
        self.assertIn("duplicate", str(ctx.exception).lower())

    def test_get_json_still_parses_valid_bytes(self):
        routes = {STAGING + "/x": body({"status": "ok"})}
        status, _, parsed = probe.get_json(self.fetch(routes), STAGING + "/x", "x")
        self.assertEqual(status, 200)
        self.assertEqual(parsed, {"status": "ok"})


if __name__ == "__main__":
    unittest.main()
