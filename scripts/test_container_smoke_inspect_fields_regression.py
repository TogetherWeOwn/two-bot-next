#!/usr/bin/env python3
"""Regression tests: malformed inspect fields FAIL cleanly; guards stay pinned.

Owned by the smoke-honesty change; supplements the protected
``test_container_smoke_fail_paths_acceptance.py`` file without editing it.

Contract under test (``scripts/container-smoke.py``):

* A ``State`` object missing ``Running``/``OOMKilled``, or an image
  inspect entry missing ``Id``/``Size``, becomes a ``RuntimeError``
  naming the check via ``smoke()``. Never a raw ``KeyError``
  traceback, never echoed Docker bodies.
* Image metadata types are validated, not just presence: a missing
  or non-object ``Config``, a present-but-non-object
  ``Healthcheck``, and a non-string ``Id`` each become a
  ``RuntimeError`` naming the field. Never ``KeyError``/
  ``AttributeError``/``TypeError`` tracebacks.
* Every new inspect guard is pinned by a fixture that turns red when
  the guard is reverted: a non-object entry (``[123]``), non-list
  inspect output, non-list/non-object image output, a ``State``
  without ``Pid``, a ``Health`` without ``Status``, and a shutdown
  ``State`` that keeps ``OOMKilled`` but omits ``Running``.

Docker and HTTP are fully mocked through the ``DockerFixture`` in
``scripts/test_container_smoke.py``: no daemon, no containers, no
network, no real sleep.

Run: PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_container_smoke_inspect_fields_regression.py' -v
"""

import contextlib
import importlib.util
import io
import json
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

RAW_CRASH_TYPES = ("Traceback", "IndexError", "KeyError", "TypeError")


def healthy_state(**overrides):
    state = {"Running": True, "Pid": 4242, "OOMKilled": False,
             "Health": {"Status": "healthy"}}
    state.update(overrides)
    return json.dumps([{"State": state}])


def state_without(*keys):
    state = {"Running": True, "Pid": 4242, "OOMKilled": False,
             "Health": {"Status": "healthy"}}
    for key in keys:
        state.pop(key)
    return json.dumps([{"State": state}])


INSPECT_NO_RUNNING = state_without("Running")
INSPECT_NO_PID = state_without("Pid")
INSPECT_HEALTH_NO_STATUS = healthy_state(Health={})
INSPECT_NON_OBJECT_ENTRY = "[123]"
INSPECT_NON_LIST = '{"State": {}}'
IMAGE_NON_LIST = '{"Id": "sha256:abc"}'
IMAGE_NON_OBJECT_ENTRY = "[123]"


def image_entry_without(*keys):
    entry = {"Id": "sha256:" + "a" * 64, "Size": 140 * smoke.MIB,
             "Config": {"User": "65532:65532",
                        "Healthcheck": {"Test": ["CMD", smoke.BINARY, "--healthcheck"]}}}
    for key in keys:
        entry.pop(key)
    return json.dumps([entry])


def image_entry_with_config(config):
    return json.dumps([{"Id": "sha256:" + "a" * 64, "Size": 140 * smoke.MIB,
                        "Config": config}])


def image_entry_with_id(image_id):
    return json.dumps([{"Id": image_id, "Size": 140 * smoke.MIB,
                        "Config": {"User": "65532:65532",
                                   "Healthcheck": {"Test": ["CMD", smoke.BINARY, "--healthcheck"]}}}])


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


class InspectFieldsRegressionTests(unittest.TestCase):
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

    def run_smoke(self, fixture, http=None):
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
                smoke.smoke("two-bot:fixture")
            except BaseException as caught:
                error = caught
        return error, out.getvalue(), err.getvalue()

    def assert_clean_fail(self, error, out, err, tokens, label):
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

    # --- missing State fields -------------------------------------------

    def test_state_missing_running_at_healthcheck_wait_fails_cleanly(self):
        # PID 1 reads a healthy State; the HEALTHCHECK wait then reads a
        # State without Running.
        fixture = InspectFixture(
            inspect_script=[healthy_state(), INSPECT_NO_RUNNING])
        error, out, err = self.run_smoke(fixture)
        self.assert_clean_fail(error, out, err, ["Running"], "no-running")

    def test_state_missing_running_during_health_wait_fails_cleanly(self):
        # The /health-wait probe (not PID 1) reads the malformed State
        # when /health needs a retry.
        calls = []

        def http(url):
            if url.endswith("/readyz"):
                return (503, parked_readyz_body())
            calls.append(url)
            if len(calls) > 1:
                return (200, {"status": "ok"})
            return (None, None)

        fixture = InspectFixture(inspect_script=[INSPECT_NO_RUNNING])
        error, out, err = self.run_smoke(fixture, http=http)
        self.assert_clean_fail(error, out, err, ["Running"], "health-wait-no-running")

    def test_shutdown_state_missing_oomkilled_fails_cleanly(self):
        # Two healthy inspects (PID 1, HEALTHCHECK wait) then a shutdown
        # read without OOMKilled.
        shutdown = json.dumps([{"State": {"Running": False, "Pid": 0}}])
        fixture = InspectFixture(
            inspect_script=[healthy_state(), healthy_state(), shutdown])
        error, out, err = self.run_smoke(fixture)
        self.assert_clean_fail(error, out, err, ["OOMKilled"], "no-oomkilled")

    def test_shutdown_state_missing_running_fails_cleanly(self):
        # The shutdown read keeps OOMKilled but omits Running, so the
        # shutdown-site Running require is the failing guard (and its
        # deletion turns this test red).
        shutdown = json.dumps([{"State": {"OOMKilled": False, "Pid": 0}}])
        fixture = InspectFixture(
            inspect_script=[healthy_state(), healthy_state(), shutdown])
        error, out, err = self.run_smoke(fixture)
        self.assert_clean_fail(error, out, err, ["Running"], "shutdown-no-running")

    # --- missing image fields --------------------------------------------

    def test_image_entry_missing_id_fails_cleanly(self):
        fixture = InspectFixture(image_inspect=image_entry_without("Id"))
        error, out, err = self.run_smoke(fixture)
        self.assert_clean_fail(error, out, err, ["Id"], "no-id")
        self.assertFalse(any(args[0] in ("create", "run") for args, _ in fixture.calls),
                         "image probe failure must precede any container")

    def test_image_entry_missing_size_fails_cleanly(self):
        fixture = InspectFixture(image_inspect=image_entry_without("Size"))
        error, out, err = self.run_smoke(fixture)
        self.assert_clean_fail(error, out, err, ["Size"], "no-size")

    def test_image_entry_missing_config_fails_cleanly(self):
        fixture = InspectFixture(image_inspect=image_entry_without("Config"))
        error, out, err = self.run_smoke(fixture)
        self.assert_clean_fail(error, out, err, ["Config"], "no-config")

    def test_image_entry_null_config_fails_cleanly(self):
        fixture = InspectFixture(image_inspect=image_entry_with_config(None))
        error, out, err = self.run_smoke(fixture)
        self.assert_clean_fail(error, out, err, ["Config"], "null-config")

    def test_image_entry_string_config_fails_cleanly(self):
        fixture = InspectFixture(image_inspect=image_entry_with_config("x"))
        error, out, err = self.run_smoke(fixture)
        self.assert_clean_fail(error, out, err, ["Config"], "string-config")

    def test_image_entry_null_healthcheck_fails_cleanly(self):
        config = {"User": "65532:65532", "Healthcheck": None}
        fixture = InspectFixture(image_inspect=image_entry_with_config(config))
        error, out, err = self.run_smoke(fixture)
        self.assert_clean_fail(error, out, err, ["Healthcheck"], "null-healthcheck")

    def test_image_entry_string_healthcheck_fails_cleanly(self):
        config = {"User": "65532:65532", "Healthcheck": "x"}
        fixture = InspectFixture(image_inspect=image_entry_with_config(config))
        error, out, err = self.run_smoke(fixture)
        self.assert_clean_fail(error, out, err, ["Healthcheck"], "string-healthcheck")

    def test_image_entry_non_string_id_fails_cleanly(self):
        # A non-string Id passes a presence-only check, then crashes the
        # real docker() with TypeError (subprocess arg must be str).
        for bad_id in (123, None, ["sha256:x"]):
            with self.subTest(id=bad_id):
                fixture = InspectFixture(image_inspect=image_entry_with_id(bad_id))
                error, out, err = self.run_smoke(fixture)
                self.assert_clean_fail(error, out, err, ["Id"], f"bad-id-{bad_id!r}")

    # --- guard pins: shapes the guards already handle ---------------------

    def test_non_object_inspect_entry_fails_cleanly(self):
        fixture = InspectFixture(inspect_script=[INSPECT_NON_OBJECT_ENTRY])
        error, out, err = self.run_smoke(fixture)
        self.assert_clean_fail(error, out, err, ["inspect"], "entry-123")

    def test_non_list_inspect_output_fails_cleanly(self):
        fixture = InspectFixture(inspect_script=[INSPECT_NON_LIST])
        error, out, err = self.run_smoke(fixture)
        self.assert_clean_fail(error, out, err, ["inspect"], "non-list-inspect")

    def test_non_list_image_inspect_fails_cleanly(self):
        fixture = InspectFixture(image_inspect=IMAGE_NON_LIST)
        error, out, err = self.run_smoke(fixture)
        self.assert_clean_fail(error, out, err, ["image"], "non-list-image")

    def test_non_object_image_entry_fails_cleanly(self):
        fixture = InspectFixture(image_inspect=IMAGE_NON_OBJECT_ENTRY)
        error, out, err = self.run_smoke(fixture)
        self.assert_clean_fail(error, out, err, ["image"], "image-entry-123")

    def test_state_missing_pid_fails_cleanly(self):
        fixture = InspectFixture(inspect_script=[INSPECT_NO_PID])
        error, out, err = self.run_smoke(fixture)
        self.assert_clean_fail(error, out, err, ["PID"], "no-pid")

    def test_health_missing_status_fails_cleanly(self):
        # No Status never reads as healthy: the HEALTHCHECK wait must fail
        # with its honest timeout, never a KeyError.
        fixture = InspectFixture(inspect_script=[INSPECT_HEALTH_NO_STATUS])
        error, out, err = self.run_smoke(fixture)
        self.assert_clean_fail(error, out, err, ["HEALTHCHECK"], "no-status")


if __name__ == "__main__":
    unittest.main()
