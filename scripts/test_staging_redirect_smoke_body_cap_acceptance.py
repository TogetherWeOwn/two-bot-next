"""Protected offline acceptance: refuse oversized redirect-smoke replies.

Missing size checks must fail these tests even when status/headers would pass.
An off-by-one refusal (>= instead of >) must fail the exact-cap controls.
Only the transport is replaced; verdicts, CLI output and receipts are real.
"""

import argparse
import contextlib
import importlib
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
smoke = importlib.import_module("staging_redirect_smoke")

STAGING = "https://two-bot-next-staging.example-sub.workers.dev"
SLUG = "staging-smoke-unknown-slug"
PATHS = {"healthz": "/healthz", "reserved": "/HEALTHZ", "unknown": "/" + SLUG}
SENTINEL = b"SYNTHETIC_BODY_SENTINEL_do_not_echo\n\x00\x1b"
HEADER_SENTINEL = "SYNTHETIC_HEADER_SENTINEL_do_not_echo"
FALLBACK_HEADERS = {"location": "https://discord.gg/fallbackCode",
                    "cache-control": "no-store"}


class RedirectSmokeBodyCapAcceptance(unittest.TestCase):
    def setUp(self):
        network = mock.patch.object(
            smoke.urllib.request.OpenerDirector, "open",
            side_effect=AssertionError("acceptance fixtures must not access the network"))
        network.start()
        self.addCleanup(network.stop)
        scratch = os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR") or os.environ.get("PAPERCLIP_SCRATCH_DIR")
        temporary = tempfile.TemporaryDirectory(dir=scratch)
        self.addCleanup(temporary.cleanup)
        self.evidence = Path(temporary.name) / "receipt.json"
        self.requested = []

    def body(self, size, prefix=SENTINEL):
        self.assertGreaterEqual(size, len(prefix))
        return prefix + b"x" * (size - len(prefix))

    def fetch(self, overrides):
        replies = {"healthz": (200, {}, b"ok\n"),
                   "reserved": (404, {}, b"not found"),
                   "unknown": (404, {}, b"not found\n")}
        replies.update(overrides)
        routes = {STAGING + PATHS[name]: reply for name, reply in replies.items()}

        def fetch_fn(url):
            self.requested.append(url)
            if url not in routes:
                raise AssertionError("unexpected offline fixture route")
            status, headers, body = routes[url]
            return status, dict(headers), body

        return fetch_fn

    def run_smoke(self, overrides):
        args = argparse.Namespace(staging_url=STAGING, unknown_slug=SLUG)
        origin, results = smoke.run(args, fetch_fn=self.fetch(overrides))
        self.assertEqual(origin, STAGING)
        self.assertEqual([result.name for result in results], ["healthz", "reserved", "unknown"])
        self.assertEqual(self.requested[-3:], [STAGING + PATHS[name]
                                              for name in ("healthz", "reserved", "unknown")])
        return {result.name: result for result in results}

    def assert_cap_failure(self, result):
        self.assertFalse(result.ok, "an over-cap reply must not produce a passing verdict")
        # The contract requires a fixed cap diagnostic, not a particular English
        # sentence. Body/header variants below must produce the identical reason.
        self.assertRegex(result.reason.lower(), r"\bcap\b")
        self.assertLessEqual(len(result.reason), 160)
        self.assertNotIn("shape", result.reason.lower())
        self.assertNotIn("SYNTHETIC_", result.reason)
        self.assertNotIn("\n", result.reason)
        self.assertNotIn("\x00", result.reason)
        self.assertNotIn("\x1b", result.reason)

    def assert_only_cap_failure(self, name, reply):
        results = self.run_smoke({name: reply})
        self.assert_cap_failure(results[name])
        for other in results.keys() - {name}:
            self.assertTrue(results[other].ok, results[other].reason)
        return results[name].reason

    def test_healthz_200_over_cap_reports_size_not_probe_shape(self):
        self.assert_only_cap_failure("healthz", (200, {}, self.body(smoke.BODY_CAP + 1)))

    def test_reserved_404_over_cap_cannot_wrongly_pass(self):
        self.assert_only_cap_failure("reserved", (404, {}, self.body(smoke.BODY_CAP + 1)))

    def test_unknown_404_over_cap_reports_size_not_not_found_shape(self):
        self.assert_only_cap_failure("unknown", (404, {}, self.body(smoke.BODY_CAP + 1)))

    def test_503_with_retry_after_over_cap_reports_size_on_each_probe(self):
        reply = (503, {"retry-after": "30"}, self.body(smoke.BODY_CAP + 1))
        for name in PATHS:
            with self.subTest(probe=name):
                self.assert_only_cap_failure(name, reply)

    def test_unknown_302_over_cap_cannot_wrongly_pass(self):
        self.assert_only_cap_failure(
            "unknown", (302, FALLBACK_HEADERS, self.body(smoke.BODY_CAP + 1)))

    def test_fail_closed_classifier_itself_refuses_over_cap_503(self):
        with self.assertRaises(smoke.SmokeError) as caught:
            smoke.fail_closed_shape(503, {"retry-after": "30"}, self.body(smoke.BODY_CAP + 1))
        self.assert_cap_failure(smoke.Result("classifier", False, str(caught.exception)))

    def test_cap_diagnostic_is_fixed_across_untrusted_body_and_header_variants(self):
        for name in PATHS:
            with self.subTest(probe=name):
                nominal_status = 200 if name == "healthz" else 404
                first = self.assert_only_cap_failure(
                    name, (nominal_status, {}, self.body(smoke.BODY_CAP + 1)))
                second = self.assert_only_cap_failure(
                    name, (503, {"retry-after": "30", "location": HEADER_SENTINEL},
                           self.body(smoke.BODY_CAP + 1, prefix=b"different synthetic body")))
                self.assertEqual(second, first)
                larger = self.assert_only_cap_failure(
                    name, (503, {"retry-after": "30"}, self.body(smoke.BODY_CAP + 17)))
                self.assertEqual(larger, first)

    def test_main_over_cap_fails_without_success_receipt_or_raw_output(self):
        for name in PATHS:
            for status in (200 if name == "healthz" else 404, 503):
                with self.subTest(probe=name, status=status):
                    out, err = io.StringIO(), io.StringIO()
                    reply = (status, {"retry-after": "30", "x-fixture": HEADER_SENTINEL},
                             self.body(smoke.BODY_CAP + 1))
                    evidence = self.evidence.with_name(f"receipt-{name}-{status}.json")
                    argv = ["--staging-url", STAGING, "--unknown-slug", SLUG,
                            "--evidence", str(evidence)]
                    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                        code = smoke.main(argv, fetch_fn=self.fetch({name: reply}))
                    self.assertEqual(code, 1)
                    self.assertFalse(evidence.exists(), "oversize must not mint a PASS receipt")
                    lines = out.getvalue().splitlines()
                    failure_lines = [line for line in lines if line.startswith("FAIL ")]
                    self.assertEqual(len(failure_lines), 1)
                    self.assertTrue(failure_lines[0].startswith("FAIL " + name + ": "))
                    self.assertRegex(failure_lines[0].lower(), r"\bcap\b")
                    self.assertEqual(lines[-1], "staging redirect smoke: 2/3 checks passed")
                    self.assertNotIn("SYNTHETIC_", out.getvalue() + err.getvalue())
                    self.assertNotIn("\x00", out.getvalue() + err.getvalue())
                    self.assertNotIn("\x1b", out.getvalue() + err.getvalue())
                    self.assertEqual(err.getvalue(), "")

    def test_reserved_404_at_real_exact_cap_still_passes(self):
        results = self.run_smoke({"reserved": (404, {}, self.body(smoke.BODY_CAP))})
        self.assertTrue(all(result.ok for result in results.values()), results)

    def test_unknown_fallback_at_real_exact_cap_still_passes(self):
        results = self.run_smoke({"unknown": (302, FALLBACK_HEADERS, self.body(smoke.BODY_CAP))})
        self.assertTrue(all(result.ok for result in results.values()), results)

    def test_healthz_exact_shape_at_cap_still_passes(self):
        # Padding healthz to 64 KiB is invalid, independently of the cap. Lower
        # the cap to the valid shape's byte length to isolate the > boundary.
        with mock.patch.object(smoke, "BODY_CAP", 3):
            reason = smoke.check_healthz(STAGING, self.fetch({}))
        self.assertEqual(reason, "healthz 200 with the exact probe shape")

    def test_unknown_404_exact_shape_at_cap_still_passes(self):
        with mock.patch.object(smoke, "BODY_CAP", 10):
            reason = smoke.check_unknown(STAGING, SLUG, self.fetch({}))
        self.assertEqual(reason, "unknown slug 404 with no redirect target")

    def test_fail_closed_shapes_at_cap_still_pass_on_all_probes(self):
        for status, headers, body, expected in [
                (503, {"retry-after": "30"}, b"redirect service misconfigured\n",
                 "503 redirect service misconfigured with retry-after 30"),
                (503, {"retry-after": "30"}, b"temporarily unavailable\n",
                 "503 temporarily unavailable with retry-after 30"),
                (500, {}, b"misconfigured campaign\n",
                 "500 misconfigured campaign without a redirect target")]:
            with self.subTest(status=status, shape=expected):
                # These shapes admit no padding. Every response is exactly the
                # temporary byte cap, so >= would incorrectly refuse all three.
                replies = {name: (status, headers, body) for name in PATHS}
                with mock.patch.object(smoke, "BODY_CAP", len(body)):
                    results = self.run_smoke(replies)
                    direct = smoke.fail_closed_shape(status, headers, body)
                self.assertEqual(direct, expected)
                for result in results.values():
                    self.assertTrue(result.ok, result.reason)
                    self.assertEqual(result.reason, expected)

    def test_main_exact_cap_valid_replies_write_a_pass_receipt(self):
        replies = {"reserved": (404, {}, self.body(smoke.BODY_CAP)),
                   "unknown": (302, FALLBACK_HEADERS, self.body(smoke.BODY_CAP))}
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = smoke.main(["--staging-url", STAGING, "--unknown-slug", SLUG,
                               "--evidence", str(self.evidence)], fetch_fn=self.fetch(replies))
        self.assertEqual(code, 0)
        receipt = json.loads(self.evidence.read_text(encoding="utf-8"))
        self.assertEqual(receipt["result"], "pass")
        self.assertEqual([(check["check"], check["verdict"]) for check in receipt["checks"]],
                         [("healthz", "pass"), ("reserved", "pass"), ("unknown", "pass")])
        self.assertNotIn("SYNTHETIC_", self.evidence.read_text(encoding="utf-8") + out.getvalue())
        self.assertEqual(err.getvalue(), "")


if __name__ == "__main__":
    unittest.main()
