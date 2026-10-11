"""Cutover acceptance probe regressions: fixture doubles only, no network or databases."""

import http.server
import io
import json
import sys
import threading
import time
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


class DuplicateComponentTest(unittest.TestCase):
    def dup_body(self, pairs, status):
        return (status, json.dumps({
            "components": [[name, value] for name, value in pairs],
            "jobs": {"counter": {"parked": True, "running": False}},
            "build_revision": "r", "build_id": "b",
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
        "build_revision": "r", "build_id": "b",
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
            "build_revision": "r", "build_id": "b",
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
            "build_revision": "r", "build_id": "b",
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
            "build_revision": "r", "build_id": "b",
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
            "build_revision": "r", "build_id": "b",
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
        self.assertIn("2 ungraded (brand_new_job, feeds)", out)
        self.assertIn("1 not yet run", out)
        self.assertIn("1 parked", out)
        self.assertNotIn("4 jobs fresh", out)


if __name__ == "__main__":
    unittest.main()
