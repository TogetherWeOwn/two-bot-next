#!/usr/bin/env python3
"""Acceptance tests: probe --timeout validation and Location no-echo.

D2 (staging probes fail closed on hostile input). Offline only: injected
transports, no network, no credentials, no databases.

Contract under test
-------------------
1. ``--timeout`` validation (both ``staging_probe.py`` and
   ``qa_cutover_probes.py``): non-finite (``nan``/``inf``, any case),
   zero, negative and over-cap values refuse with a FIXED message and a
   nonzero exit before any request is attempted. Fixed means the refusal
   text is byte-identical across every invalid fixture, names
   ``--timeout``, and never echoes the supplied value. ``staging_probe``
   uses its established refusal exit (2); the cutover probe uses any
   nonzero exit. The over-cap boundary itself is the Builder's documented
   choice; these tests only require that ``1e9`` seconds (31 years) is
   over it and that ``0.5``..``30`` stay valid, with defaults unchanged
   (20.0 / 10).
2. Location no-echo (``staging_probe.py``): raw server ``Location``
   values never appear in the JSON report, in failure text, or on
   stdout/stderr. Request entries record presence/shape only, the
   fallback observation carries a shape token instead of the raw target,
   and the unknown-slug failure names the check without the target.
   Existing verdicts are unchanged (404/no-Location checks, fallback
   configured/unset, unknown-slug fallback). The cutover probe's
   transport drops headers by construction, so its redirect refusal is
   pinned as a control.
3. RED is behavioral: every failing assertion below fails on a verdict,
   exit code, call count or report-text comparison against live code
   paths, never on an import error, missing fixture or refusal to run.

Failing assertions use fixed messages so a red run never echoes a
sentinel into the test log. Sentinels are assembled from fragments at
runtime; no credential-shaped literal is stored in this repository.

Protected file: the Builder builds on this commit and must never edit
this file; test corrections go back to its author.

Run: PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_probe_timeout_location_acceptance.py' -v
"""

import contextlib
import io
import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import qa_cutover_probes as cutover
import staging_probe

STAGING = "https://two-bot-next-staging.5150.workers.dev"
VERSION = "f8b7cc48-cdc4-4951-b498-8b25ce7e8664"
SLUG = "probe-0123456789ab"
STAGING_READY = {
    "components": [["process", "ready"], ["gateway", "ready"], ["database", "ready"]],
    "jobs": {},
}

# --timeout fixtures: every value here must refuse. "1e9" is over any sane
# per-request cap; "NaN"/"Infinity" force numeric (math.isfinite) validation
# rather than case-sensitive string matching.
INVALID_TIMEOUTS = (
    "nan", "NaN", "inf", "-inf", "Infinity", "0", "0.0", "-0", "-1", "-0.5", "1e9",
)
VALID_TIMEOUTS = ("0.5", "1", "5", "20", "30")

# Hostile Location values, assembled from fragments so no credential-shaped
# literal is stored here.
CRED_CORE = "Cred" + "NeverEcho" + "4L"
CTL_CORE = "Ctl" + "NeverEcho" + "5M"
FB_CORE = "Fallback" + "NeverEcho" + "6N"
SLUG_CORE = "Slug" + "NeverEcho" + "7P"

CRED_URL = "https" + "://" + "user" + ":" + CRED_CORE + "@" + "evil.invalid" + "/cb"
CTL_URL = "https" + "://" + "evil.invalid" + "/" + "\x1b[2J" + CTL_CORE
FB_URL = "https://discord.gg/" + FB_CORE
SLUG_URL = "https" + "://" + "evil.invalid" + "/" + SLUG_CORE

LOCATION_FRAGMENTS = (CRED_URL, CTL_URL, FB_URL, SLUG_URL,
                      CRED_CORE, CTL_CORE, FB_CORE, SLUG_CORE)


def staging_healthy_fetch(method, path):
    """A fully green staging surface with no Location headers anywhere."""
    text = {"content-type": "text/plain"}
    if path == "/health":
        return staging_probe.Reply(200, {"content-type": "application/json"},
                                   '{"status":"ok"}')
    if path == "/readyz":
        return staging_probe.Reply(200, {"content-type": "application/json"},
                                   json.dumps(STAGING_READY))
    if path == "/healthz":
        return staging_probe.Reply(200, text, "ok\n")
    if path == "/ops/metrics":
        return staging_probe.Reply(404, text, "not found")
    if path in staging_probe.HEALTHZ_ALIASES or path in staging_probe.METRICS_ALIASES:
        return staging_probe.Reply(404, text, "not found\n")
    if method == "HEAD" and path == "/":
        return staging_probe.Reply(404, text, "not found\n")
    return staging_probe.Reply(404, text, "not found\n")


def cutover_healthy_double():
    health = (200, json.dumps({"status": "ok"}).encode())
    readyz = (200, json.dumps({
        "components": [["process", "ready"], ["gateway", "ready"]],
        "jobs": {"counter": {"parked": False, "running": True}},
        "build_revision": "0123456789abcdef0123456789abcdef01234567", "build_id": "700-1",
    }).encode())
    calls = []

    def fetch(url):
        calls.append(url)
        if url.endswith("/health"):
            return health
        if url.endswith("/readyz"):
            return readyz
        raise AssertionError(f"unexpected GET {url}")

    fetch.calls = calls
    return fetch


class NoEchoAssertions:
    def assert_clean(self, fragments, texts, label):
        for fragment in fragments:
            for text in texts:
                # Fixed message only: printing the fragment or the text here
                # would itself leak the sentinel into the test log.
                self.assertFalse(fragment in text, f"echoed Location fragment ({label})")


class StagingProbeTimeoutTests(unittest.TestCase):
    def run_main(self, timeout_value):
        out, err = io.StringIO(), io.StringIO()
        with mock.patch.object(staging_probe, "make_fetch") as transport, \
                contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            transport.side_effect = lambda origin, timeout: staging_healthy_fetch
            # `=` form: argparse only exempts digit-negatives (`-1`) from
            # option parsing, so `-inf` must ride on `=` to reach validation.
            code = staging_probe.main(["--base-url", STAGING,
                                       "--worker-version", VERSION,
                                       f"--timeout={timeout_value}"])
        return code, out.getvalue(), err.getvalue(), transport

    def test_invalid_timeouts_refuse_before_any_request(self):
        for value in INVALID_TIMEOUTS:
            with self.subTest(timeout=value):
                code, _out, _err, transport = self.run_main(value)
                self.assertEqual(code, 2, f"--timeout {value} must refuse with exit 2")
                transport.assert_not_called()

    def test_invalid_timeout_refusal_is_fixed_and_names_the_flag(self):
        refusals = {}
        for value in INVALID_TIMEOUTS:
            with self.subTest(timeout=value):
                code, out, err, _transport = self.run_main(value)
                self.assertEqual(code, 2, f"--timeout {value} must refuse with exit 2")
                self.assertEqual(out, "", "a refusal must not write a report to stdout")
                self.assertIn("--timeout", err, "the refusal must name --timeout")
                refusals[value] = err
        texts = list(refusals.values())
        for value, text in refusals.items():
            with self.subTest(timeout=value):
                self.assertEqual(text, texts[0],
                                 "the refusal must be byte-identical across invalid values")

    def test_valid_timeouts_pass_through_byte_identically(self):
        for value in VALID_TIMEOUTS:
            with self.subTest(timeout=value):
                out, err = io.StringIO(), io.StringIO()
                with mock.patch.object(staging_probe, "make_fetch") as transport, \
                        contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                    transport.side_effect = lambda origin, timeout: staging_healthy_fetch
                    code = staging_probe.main(["--base-url", STAGING,
                                               "--worker-version", VERSION,
                                               "--timeout", value])
                self.assertEqual(code, 0, f"--timeout {value} must stay valid")
                transport.assert_called_once_with(STAGING, float(value))
                self.assertEqual(json.loads(out.getvalue())["verdict"], "PASS")

    def test_default_timeout_is_unchanged(self):
        out = io.StringIO()
        with mock.patch.object(staging_probe, "make_fetch") as transport, \
                contextlib.redirect_stdout(out), contextlib.redirect_stderr(io.StringIO()):
            transport.side_effect = lambda origin, timeout: staging_healthy_fetch
            code = staging_probe.main(["--base-url", STAGING,
                                       "--worker-version", VERSION])
        self.assertEqual(code, 0)
        transport.assert_called_once_with(STAGING, 20.0)


class CutoverProbeTimeoutTests(unittest.TestCase):
    def run_main(self, timeout_value):
        fetch = cutover_healthy_double()
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            # `=` form: see the staging-probe note on `-inf` vs argparse.
            code = cutover.main(["--base-url", "http://h/", f"--timeout={timeout_value}"],
                                fetch_fn=fetch)
        return code, out.getvalue(), err.getvalue(), fetch

    def test_invalid_timeouts_refuse_before_any_request(self):
        for value in INVALID_TIMEOUTS:
            with self.subTest(timeout=value):
                code, _out, _err, fetch = self.run_main(value)
                self.assertNotEqual(code, 0, f"--timeout {value} must refuse nonzero")
                self.assertEqual(fetch.calls, [], "a refusal must attempt zero requests")

    def test_invalid_timeout_refusal_is_fixed_and_names_the_flag(self):
        refusals = {}
        for value in INVALID_TIMEOUTS:
            with self.subTest(timeout=value):
                code, out, err, fetch = self.run_main(value)
                self.assertNotEqual(code, 0, f"--timeout {value} must refuse nonzero")
                self.assertEqual(fetch.calls, [], "a refusal must attempt zero requests")
                combined = out + err
                self.assertIn("--timeout", combined, "the refusal must name --timeout")
                self.assertNotIn("PASS", combined, "a refusal must not print check lines")
                self.assertNotIn("FAIL", combined, "a refusal must not print check lines")
                refusals[value] = combined
        texts = list(refusals.values())
        for value, text in refusals.items():
            with self.subTest(timeout=value):
                self.assertEqual(text, texts[0],
                                 "the refusal must be byte-identical across invalid values")

    def test_valid_timeouts_pass_controls(self):
        for value in VALID_TIMEOUTS:
            with self.subTest(timeout=value):
                code, out, _err, fetch = self.run_main(value)
                self.assertEqual(code, 0, f"--timeout {value} must stay valid: {out}")
                self.assertEqual(len(fetch.calls), 2, "both routes must be fetched")

    def test_default_timeout_passes_control(self):
        fetch = cutover_healthy_double()
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = cutover.main(["--base-url", "http://h/"], fetch_fn=fetch)
        self.assertEqual(code, 0, out.getvalue())
        self.assertEqual(len(fetch.calls), 2, "both routes must be fetched")


class LocationNoEchoTests(NoEchoAssertions, unittest.TestCase):
    def test_request_entries_carry_no_raw_location(self):
        def hostile(method, path):
            return staging_probe.Reply(200, {"location": CRED_URL}, '{"status":"ok"}')

        seen = []
        probe = staging_probe.Probe("health", hostile, seen)
        staging_probe.probe_health(probe)
        result = probe.result()
        self.assertEqual(result["verdict"], "PASS")
        self.assert_clean(LOCATION_FRAGMENTS, [json.dumps(result)], "request entry")

        # Presence must stay observable: the same reply without a Location
        # header records a distinguishable entry, still with no raw value.
        def plain(method, path):
            return staging_probe.Reply(200, {}, '{"status":"ok"}')

        plain_probe = staging_probe.Probe("health", plain, [])
        staging_probe.probe_health(plain_probe)
        plain_result = plain_probe.result()
        self.assert_clean(LOCATION_FRAGMENTS, [json.dumps(plain_result)], "plain entry")
        self.assertNotEqual(result["requests"], plain_result["requests"],
                            "a Location-bearing reply must stay distinguishable")

    def test_control_bearing_location_still_fails_cleanly(self):
        def hostile(method, path):
            if path == "/health":
                return staging_probe.Reply(200, {"location": CTL_URL}, '{"status":"ok"}')
            return staging_healthy_fetch(method, path)

        seen = []
        probe = staging_probe.Probe("health", hostile, seen)
        staging_probe.probe_health(probe)
        self.assert_clean(LOCATION_FRAGMENTS,
                          [json.dumps(probe.result())], "control-char Location")

    def test_fallback_observation_carries_a_shape_token_not_the_target(self):
        def hostile(method, path):
            if method == "HEAD" and path == "/":
                return staging_probe.Reply(
                    302, {"location": FB_URL, "cache-control": "no-store"}, "")
            return staging_probe.Reply(404, {"content-type": "text/plain"}, "not found\n")

        probe = staging_probe.Probe("fallback", hostile, [])
        staging_probe.probe_fallback(probe, SLUG, None)
        result = probe.result()
        self.assertEqual(result["verdict"], "PASS")
        self.assertEqual(result["observed"].get("fallback"), "configured")
        self.assert_clean(LOCATION_FRAGMENTS, [json.dumps(result)], "fallback observation")

    def test_unknown_slug_failure_names_the_check_without_the_target(self):
        def hostile(method, path):
            if method == "HEAD" and path == f"/{SLUG}":
                return staging_probe.Reply(302, {"location": SLUG_URL}, "")
            return staging_probe.Reply(404, {"content-type": "text/plain"}, "not found\n")

        probe = staging_probe.Probe("fallback", hostile, [])
        staging_probe.probe_fallback(probe, SLUG, None)
        result = probe.result()
        self.assertEqual(result["verdict"], "FAIL")
        failures = result["failures"]
        self.assertTrue(any(f"HEAD /{SLUG}" in f and "got 302" in f for f in failures),
                        "the failure must still name the check and status")
        self.assert_clean(LOCATION_FRAGMENTS, [json.dumps(failures)], "failure text")

    def hostile_surface(self, method, path):
        if path == "/health":
            return staging_probe.Reply(200, {"location": CRED_URL}, '{"status":"ok"}')
        if path == "/readyz":
            return staging_probe.Reply(200, {"content-type": "application/json"},
                                       json.dumps(STAGING_READY))
        if path == "/healthz":
            return staging_probe.Reply(200, {"content-type": "text/plain"}, "ok\n")
        if path == "/ops/metrics":
            return staging_probe.Reply(404, {"content-type": "text/plain"}, "not found")
        if path in staging_probe.HEALTHZ_ALIASES or path in staging_probe.METRICS_ALIASES:
            return staging_probe.Reply(404, {"content-type": "text/plain"}, "not found\n")
        if method == "HEAD" and path == "/":
            return staging_probe.Reply(
                302, {"location": FB_URL, "cache-control": "no-store"}, "")
        if method == "HEAD":
            # The unknown slug (the only other HEAD target) redirects away
            # from the configured fallback, which fails the check.
            return staging_probe.Reply(302, {"location": SLUG_URL}, "")
        return staging_probe.Reply(404, {"content-type": "text/plain"}, "not found\n")

    def run_main_hostile(self, extra_args):
        out, err = io.StringIO(), io.StringIO()
        with mock.patch.object(staging_probe, "fence", return_value=STAGING), \
                mock.patch.object(staging_probe, "make_fetch",
                                  return_value=self.hostile_surface), \
                contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = staging_probe.main(["--base-url", STAGING,
                                       "--worker-version", VERSION,
                                       "--timeout", "5", *extra_args])
        return code, out.getvalue(), err.getvalue()

    def test_main_report_file_and_streams_carry_no_location(self):
        with tempfile.TemporaryDirectory() as scratch:
            report_path = Path(scratch) / "report.json"
            code, out, err = self.run_main_hostile(["--report", str(report_path)])
            report_text = report_path.read_text()
        self.assertEqual(code, 1, "the hostile fallback surface must FAIL")
        self.assert_clean(LOCATION_FRAGMENTS, [report_text, out, err],
                          "report file and streams")

    def test_main_stdout_report_carries_no_location(self):
        code, out, err = self.run_main_hostile([])
        self.assertEqual(code, 1, "the hostile fallback surface must FAIL")
        report = json.loads(out)
        fallback = next(p for p in report["probes"] if p["name"] == "fallback")
        self.assertEqual(fallback["verdict"], "FAIL")
        self.assertTrue(any("got 302" in f for f in fallback["failures"]),
                        "the failure must still name the check and status")
        self.assert_clean(LOCATION_FRAGMENTS, [out, err], "stdout report and streams")

    def test_404_no_location_checks_unchanged(self):
        probe = staging_probe.Probe("fallback", staging_healthy_fetch, [])
        staging_probe.probe_fallback(probe, SLUG, None)
        result = probe.result()
        self.assertEqual(result["verdict"], "PASS")
        self.assertEqual(result["observed"],
                         {"fallback": "unset", "unknown_slug": "not_found"})

    def test_404_with_location_still_fails_with_fixed_wording(self):
        def hostile(method, path):
            if path == staging_probe.HEALTHZ_ALIASES[0]:
                return staging_probe.Reply(302, {"location": SLUG_URL}, "")
            return staging_healthy_fetch(method, path)

        probe = staging_probe.Probe("reserved_healthz", hostile, [])
        staging_probe.probe_reserved_healthz(probe)
        result = probe.result()
        self.assertEqual(result["verdict"], "FAIL")
        failures = json.dumps(result["failures"])
        self.assertIn("expected 404, got 302", failures)
        self.assert_clean((SLUG_URL, SLUG_CORE), [failures], "404 failure wording")

    def test_configured_fallback_verdict_unchanged(self):
        def configured(method, path):
            if method == "HEAD" and path == "/":
                return staging_probe.Reply(
                    302, {"location": "https://discord.gg/twoInvite",
                          "cache-control": "no-store"}, "")
            return staging_probe.Reply(404, {"content-type": "text/plain"}, "not found\n")

        probe = staging_probe.Probe("fallback", configured, [])
        staging_probe.probe_fallback(probe, SLUG, "twoInvite")
        result = probe.result()
        self.assertEqual(result["verdict"], "PASS", result["failures"])
        self.assertEqual(result["observed"].get("fallback"), "configured")


class CutoverRedirectRefusalControl(unittest.TestCase):
    """The cutover transport drops headers by construction, so a redirect
    refusal can only ever name the check, never a target. This pins that."""

    def test_readyz_redirect_refusal_names_no_target(self):
        health = (200, json.dumps({"status": "ok"}).encode())
        redirect = (302, b"{}")

        def fetch(url):
            return health if url.endswith("/health") else redirect

        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = cutover.main(["--base-url", "http://h/"], fetch_fn=fetch)
        text = out.getvalue()
        self.assertEqual(code, 1)
        self.assertIn("refusing to follow a redirect", text)
        self.assertNotIn("http", text)


if __name__ == "__main__":
    unittest.main()
