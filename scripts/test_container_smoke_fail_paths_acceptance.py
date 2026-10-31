#!/usr/bin/env python3
"""Acceptance tests: container smoke FAILs cleanly on malformed inspect output.

Protected acceptance file: the implementation change must satisfy these
tests without editing them. Test defects are corrected here, not by
weakening assertions in the build.

Contract under test (``scripts/container-smoke.py``):

* Malformed ``docker`` inspect output -- an empty ``image inspect``
  array, an empty ``inspect`` array, and ``inspect`` entries missing
  ``State`` or ``Health`` -- becomes a ``RuntimeError`` naming the
  check via ``smoke()`` and ``FAIL container smoke: ...`` naming the
  check via ``main()``. Never an ``IndexError``/``KeyError``/
  ``TypeError`` traceback, never echoed Docker bodies.
* Every inspect site is covered: the image probe, the PID 1 probe, the
  /health-wait probe, the HEALTHCHECK-wait probe, and the shutdown
  probe.
* The Docker HEALTHCHECK wait enforces its own full documented 30s
  budget measured from its own start: time the /health loop already
  consumed cannot shrink it, and the failure text still states the 30s
  budget.
* Valid smoke behavior is unchanged (passing controls).

Docker and HTTP are fully mocked through the ``DockerFixture`` in
``scripts/test_container_smoke.py``: no daemon, no containers, no
network, no real sleep.

Run: PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_container_smoke_fail_paths_acceptance.py' -v
"""

import contextlib
import importlib.util
import io
import os
from pathlib import Path
import subprocess
import sys
import unittest
from unittest.mock import patch

_spec = importlib.util.spec_from_file_location(
    "test_container_smoke_base", Path(__file__).with_name("test_container_smoke.py")
)
_base = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_base)

smoke = _base.smoke
DockerFixture = _base.DockerFixture
parked_readyz_body = _base.parked_readyz_body

IMAGE_EMPTY = "[]"
INSPECT_EMPTY = "[]"
INSPECT_NO_STATE = '[{"Name": "/two-bot-smoke-x", "Id": "abc123"}]'
INSPECT_NO_HEALTH = (
    '[{"State": {"Running": true, "Pid": 4242, "OOMKilled": false}}]'
)

RAW_CRASH_TYPES = ("Traceback", "IndexError", "KeyError", "TypeError")


class InspectFixture(DockerFixture):
    """DockerFixture with scripted raw inspect stdout.

    ``image_inspect`` replaces ``docker image inspect`` stdout;
    ``inspect_script`` is a list of stdout strings served in order to
    successive ``docker inspect`` calls (the last entry repeats).
    ``None`` keeps the default healthy output for that probe.
    """

    def __init__(self, image_inspect=None, inspect_script=None):
        super().__init__()
        self.image_inspect = image_inspect
        self.inspect_script = list(inspect_script or [])
        self.inspect_calls = 0

    def __call__(self, *args, **kwargs):
        if args[:2] == ("image", "inspect") and self.image_inspect is not None:
            self.calls.append((args, kwargs))
            return subprocess.CompletedProcess(args, 0, self.image_inspect, "")
        if args[0] == "inspect" and self.inspect_script:
            self.calls.append((args, kwargs))
            index = min(self.inspect_calls, len(self.inspect_script) - 1)
            self.inspect_calls += 1
            return subprocess.CompletedProcess(args, 0, self.inspect_script[index], "")
        return super().__call__(*args, **kwargs)


class FailPathsAcceptanceTests(unittest.TestCase):
    def setUp(self):
        # Any real subprocess is a test failure: docker is always stubbed.
        guard = patch.object(
            smoke.subprocess, "run",
            side_effect=AssertionError("real subprocess in an offline test"))
        guard.start()
        self.addCleanup(guard.stop)
        self.clock = [1000.0]

    def monotonic(self):
        return self.clock[0]

    def sleep(self, seconds):
        self.clock[0] += seconds

    def healthy_http(self, url):
        if url.endswith("/readyz"):
            return (503, parked_readyz_body())
        return (200, {"status": "ok"})

    def run_smoke(self, fixture, http=None, via_main=False):
        """Run the smoke and capture (exception, stdout, stderr)."""
        error = None
        http = http or self.healthy_http
        with patch.object(smoke, "docker", fixture), patch.object(
            smoke, "http_response", side_effect=http
        ), patch.object(smoke.time, "monotonic", side_effect=self.monotonic), patch.object(
            smoke.time, "sleep", side_effect=self.sleep
        ), patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": ""}), contextlib.redirect_stdout(
            io.StringIO()
        ) as out, contextlib.redirect_stderr(io.StringIO()) as err:
            try:
                if via_main:
                    with patch.object(sys, "argv", ["container-smoke.py", "two-bot:fixture"]):
                        smoke.main()
                else:
                    smoke.smoke("two-bot:fixture")
            except BaseException as caught:  # SystemExit included on purpose
                error = caught
        return error, out.getvalue(), err.getvalue()

    # --- helpers --------------------------------------------------------

    def combined(self, error, out, err):
        return "\n".join([out, err, str(error)])

    def assert_clean_direct_fail(self, error, out, err, tokens, label):
        self.assertIsInstance(
            error, RuntimeError,
            f"malformed inspect must raise RuntimeError, got {type(error).__name__} "
            f"in case {label}")
        for crash in RAW_CRASH_TYPES:
            self.assertNotIn(
                crash, type(error).__name__ + str(error),
                f"raw crash type leaks in case {label}")
        for token in tokens:
            self.assertIn(token, str(error),
                          f"failure does not name {token!r} in case {label}")

    def assert_clean_cli_fail(self, error, out, err, tokens, label):
        self.assertIsInstance(
            error, SystemExit, f"CLI must exit via SystemExit in case {label}")
        code = str(error.code)
        self.assertTrue(code.startswith("FAIL container smoke:"),
                        f"CLI must use the FAIL contract in case {label}: {code!r}")
        for token in tokens:
            self.assertIn(token, code, f"CLI failure does not name {token!r} in {label}")
        for crash in RAW_CRASH_TYPES:
            self.assertNotIn(crash, code + out + err,
                             f"raw crash type leaks in CLI case {label}")

    # --- empty image inspect --------------------------------------------

    def test_empty_image_inspect_fails_cleanly(self):
        fixture = InspectFixture(image_inspect=IMAGE_EMPTY)
        error, out, err = self.run_smoke(fixture)
        self.assert_clean_direct_fail(error, out, err, ["image", "inspect"], "image-empty")
        self.assertFalse(any(args[0] in ("create", "run") for args, _ in fixture.calls),
                         "image probe failure must precede any container")

    def test_empty_image_inspect_fails_cleanly_via_cli(self):
        fixture = InspectFixture(image_inspect=IMAGE_EMPTY)
        error, out, err = self.run_smoke(fixture, via_main=True)
        self.assert_clean_cli_fail(error, out, err, ["image"], "cli-image-empty")

    # --- empty container inspect ----------------------------------------

    def test_empty_container_inspect_fails_cleanly(self):
        fixture = InspectFixture(inspect_script=[INSPECT_EMPTY])
        error, out, err = self.run_smoke(fixture)
        self.assert_clean_direct_fail(error, out, err, ["inspect"], "inspect-empty")

    def test_empty_container_inspect_fails_cleanly_via_cli(self):
        fixture = InspectFixture(inspect_script=[INSPECT_EMPTY])
        error, out, err = self.run_smoke(fixture, via_main=True)
        self.assert_clean_cli_fail(error, out, err, ["inspect"], "cli-inspect-empty")

    def test_empty_inspect_during_health_wait_fails_cleanly(self):
        # The /health-wait probe (not PID 1) is the first inspect reader
        # when /health needs a retry.
        calls = []

        def http(url):
            if url.endswith("/readyz"):
                return (503, parked_readyz_body())
            calls.append(url)
            if len(calls) > 1:
                return (200, {"status": "ok"})
            return (None, None)

        fixture = InspectFixture(inspect_script=[INSPECT_EMPTY])
        error, out, err = self.run_smoke(fixture, http=http)
        self.assert_clean_direct_fail(error, out, err, ["inspect"], "health-wait-empty")

    def test_empty_inspect_at_shutdown_check_fails_cleanly(self):
        # Two healthy inspects (PID 1, HEALTHCHECK wait) then a malformed
        # shutdown read.
        healthy = (
            '[{"State": {"Running": true, "Pid": 4242, "OOMKilled": false, '
            '"Health": {"Status": "healthy"}}}]'
        )
        fixture = InspectFixture(inspect_script=[healthy, healthy, INSPECT_EMPTY])
        error, out, err = self.run_smoke(fixture)
        self.assert_clean_direct_fail(error, out, err, ["inspect"], "shutdown-empty")

    # --- missing State / Health ------------------------------------------

    def test_inspect_missing_state_fails_cleanly(self):
        fixture = InspectFixture(inspect_script=[INSPECT_NO_STATE])
        error, out, err = self.run_smoke(fixture)
        self.assert_clean_direct_fail(error, out, err, ["State"], "missing-state")

    def test_inspect_missing_health_fails_cleanly(self):
        fixture = InspectFixture(inspect_script=[INSPECT_NO_HEALTH])
        error, out, err = self.run_smoke(fixture)
        self.assert_clean_direct_fail(error, out, err, ["Health"], "missing-health")

    def test_inspect_missing_health_fails_cleanly_via_cli(self):
        fixture = InspectFixture(inspect_script=[INSPECT_NO_HEALTH])
        error, out, err = self.run_smoke(fixture, via_main=True)
        self.assert_clean_cli_fail(error, out, err, ["Health"], "cli-missing-health")

    # --- HEALTHCHECK deadline honesty -------------------------------------

    def test_healthcheck_wait_gets_its_own_full_budget(self):
        # /health consumes ~25s of the clock, then the HEALTHCHECK wait
        # needs ~10s of its own. An honest independent 30s budget passes;
        # a budget shared with (and drained by) the /health loop fails.
        fixture = DockerFixture()
        health_calls = [0]

        def http(url):
            if url.endswith("/readyz"):
                return (503, parked_readyz_body())
            health_calls[0] += 1
            if self.clock[0] < 1025.0 and health_calls[0] < 10000:
                return (None, None)
            return (200, {"status": "ok"})

        healthcheck_reads = [0]

        def docker(*args, **kwargs):
            if args[0] == "inspect":
                healthcheck_reads[0] += 1
                if self.clock[0] >= 1035.0 or healthcheck_reads[0] >= 10000:
                    fixture.health_status = "healthy"
                else:
                    fixture.health_status = "starting"
            return fixture(*args, **kwargs)

        error, out, _ = self.run_smoke(docker, http=http)
        self.assertIsNone(error, f"independent HEALTHCHECK budget must pass, got: {error!r}")
        self.assertGreaterEqual(self.clock[0], 1035.0)
        self.assertIn("SIGTERM exits 0", out)

    def test_healthcheck_timeout_states_the_documented_budget(self):
        fixture = DockerFixture()
        fixture.health_status = "unhealthy"
        error, out, err = self.run_smoke(fixture)
        self.assertIsInstance(error, RuntimeError)
        self.assertIn("30s", str(error),
                      "HEALTHCHECK timeout must state its documented 30s budget")

    # --- passing controls ---------------------------------------------------

    def test_control_valid_smoke_passes(self):
        error, out, _ = self.run_smoke(DockerFixture())
        self.assertIsNone(error, f"valid smoke must pass, got: {error!r}")
        self.assertIn("SIGTERM exits 0", out)

    def test_control_eventual_healthcheck_healthy_passes(self):
        reads = [0]
        fixture = DockerFixture()
        fixture.health_status = "starting"

        def docker(*args, **kwargs):
            if args[0] == "inspect":
                reads[0] += 1
                if reads[0] >= 4:
                    fixture.health_status = "healthy"
            return fixture(*args, **kwargs)

        error, out, _ = self.run_smoke(docker)
        self.assertIsNone(error, f"briefly-starting HEALTHCHECK must pass, got: {error!r}")
        self.assertIn("SIGTERM exits 0", out)


if __name__ == "__main__":
    unittest.main()
