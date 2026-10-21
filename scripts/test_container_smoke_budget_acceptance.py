#!/usr/bin/env python3
"""Acceptance tests: container smoke refuses unusable size budgets.

Protected acceptance file: the implementation change must satisfy these
tests without editing them. Test defects are corrected here, not by
weakening assertions in the build.

Contract under test (``scripts/container-smoke.py`` owns
``--image-max-bytes``/``--binary-max-bytes``):

* Non-positive budgets (``0``, ``-1``, ...) refuse with a fixed message
  and nonzero exit before any Docker call. A ``0`` budget must not be
  graded as "image exceeds size budget" after history inspection.
* Over-cap budgets (gigantic values that would silently disable the
  budget) refuse the same way, with zero Docker invocations. The exact
  upper bound is the builder's choice; these tests pin only that
  10 GiB and 2**62 refuse while defaults and small sane values pass
  through. Any reasonable cap (image <= 1 GiB, binary <= 256 MiB)
  satisfies both sides.
* Refusal uses the existing CLI contract: ``SystemExit`` whose code is
  a short ``str`` starting with ``FAIL container smoke:``, naming the
  offending flag (``--image-max-bytes`` or ``--binary-max-bytes``) and
  fixed refusal vocabulary (refusing/invalid/must be/positive). It never
  echoes the supplied number for over-cap values, never prints a
  traceback, and stays bounded (at most 300 characters).
* Sane budgets pass through byte-identically: defaults, exact fixture
  sizes and small explicit values reach Docker; tiny ``1``-byte budgets
  still fail with the existing "exceeds size budget" comparison (not a
  refusal) after invoking Docker.

Docker and HTTP are fully mocked through the ``DockerFixture`` in
``scripts/test_container_smoke.py``: no daemon, no containers, no
network (not even loopback).

Run: PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_container_smoke_budget_acceptance.py' -v
"""

import contextlib
import importlib.util
import io
import os
from pathlib import Path
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

MAX_DIAGNOSTIC_LINE = 300
TEN_GIB = 10 * 1024 * smoke.MIB
HUGE = 2**62
NONPOSITIVE = (0, -1, -100, -2147483648)
OVER_CAP = (TEN_GIB, 1099511627776, HUGE)


def refusal_tokens(flag):
    return (flag,)


class BudgetAcceptanceTests(unittest.TestCase):
    def setUp(self):
        self.fixture = DockerFixture()
        self.clock = 0
        self.http_calls = []
        parked = parked_readyz_body()

        def http(url):
            self.http_calls.append(url)
            if url.endswith("/readyz"):
                return (503, parked)
            return (200, {"status": "ok"})

        self.http = http

    def tick(self):
        self.clock += 1
        return self.clock

    def run_cli(self, argv):
        """Run smoke.main() with mocked Docker/HTTP/time; capture outcome."""
        error = None
        docker_calls = []

        def counting_docker(*args, **kwargs):
            docker_calls.append((args, kwargs))
            return self.fixture(*args, **kwargs)

        with patch.object(smoke, "docker", counting_docker), patch.object(
            smoke, "http_response", side_effect=lambda url: self.http(url)
        ), patch.object(smoke.time, "monotonic", side_effect=self.tick), patch.object(
            smoke.time, "sleep"
        ), patch.dict(
            os.environ, {"GITHUB_STEP_SUMMARY": ""}
        ), patch.object(
            sys, "argv", argv
        ), contextlib.redirect_stdout(
            io.StringIO()
        ) as out, contextlib.redirect_stderr(
            io.StringIO()
        ) as err:
            try:
                smoke.main()
            except BaseException as caught:  # SystemExit included on purpose
                error = caught
        return error, out.getvalue(), err.getvalue(), docker_calls

    def run_budget_cli(self, image_max=None, binary_max=None):
        argv = ["container-smoke.py", "two-bot:fixture"]
        if image_max is not None:
            argv += ["--image-max-bytes", str(image_max)]
        if binary_max is not None:
            argv += ["--binary-max-bytes", str(binary_max)]
        return self.run_cli(argv)

    # --- refusal assertions ------------------------------------------------

    def assert_refuses_before_docker(self, error, out, err, calls, value, flag, label):
        self.assertIsInstance(
            error, SystemExit, f"unusable budget must exit, not return, in case {label}"
        )
        code = error.code
        self.assertIsInstance(code, str, f"exit code must be text in case {label}")
        self.assertTrue(
            code.startswith("FAIL container smoke:"),
            f"refusal must use the FAIL container smoke contract in case {label}: {code!r}",
        )
        self.assertLessEqual(
            len(code), MAX_DIAGNOSTIC_LINE, f"refusal line is unbounded in case {label}"
        )
        lowered = code.lower()
        self.assertIn(
            flag, lowered, f"refusal does not name {flag} in case {label}: {code!r}"
        )
        self.assertTrue(
            any(token in lowered for token in ("refus", "invalid", "must be", "positive")),
            f"refusal lacks fixed vocabulary in case {label}: {code!r}",
        )
        # Over-cap numbers must not be echoed back into the diagnostic.
        if value is not None and value >= TEN_GIB:
            self.assertNotIn(
                str(value), code, f"refusal echoes the supplied budget in case {label}"
            )
        self.assertEqual(
            calls, [], f"refusal must precede any Docker call in case {label}: {calls!r}"
        )
        self.assertEqual(
            self.http_calls, [], f"refusal must precede any HTTP read in case {label}"
        )
        self.assertNotIn("Traceback", out + err, f"traceback printed in case {label}")

    def assert_passes_through(self, error, out, err, calls, label):
        self.assertIsNone(error, f"sane budget must pass, got {error!r} in case {label}")
        self.assertGreater(len(calls), 0, f"sane budget never reached Docker in case {label}")
        self.assertEqual(calls[0][0][:2], ("image", "inspect"))
        self.assertIn("SIGTERM exits 0", out, f"smoke did not complete in case {label}")

    def assert_budget_comparison(self, error, out, err, calls, fragment, label):
        # Tiny but valid budgets reach the size comparison (not a refusal).
        self.assertIsInstance(
            error, SystemExit, f"Tiny budget must exit nonzero in case {label}"
        )
        code = str(error.code)
        self.assertIn(
            fragment, code, f"Tiny budget did not reach comparison in case {label}: {code!r}"
        )
        self.assertGreater(len(calls), 0, f"Tiny budget never reached Docker in case {label}")
        self.assertNotIn("Traceback", out + err, f"traceback printed in case {label}")

    # --- image budgets -----------------------------------------------------

    def test_image_zero_refuses_before_docker(self):
        error, out, err, calls = self.run_budget_cli(image_max=0)
        self.assert_refuses_before_docker(
            error, out, err, calls, 0, "--image-max-bytes", "image-0"
        )

    def test_image_negative_refuses_before_docker(self):
        for value in (-1, -100, -2147483648):
            with self.subTest(value=value):
                self.fixture = DockerFixture()
                self.http_calls = []
                error, out, err, calls = self.run_budget_cli(image_max=value)
                self.assert_refuses_before_docker(
                    error, out, err, calls, value, "--image-max-bytes", f"image-{value}"
                )

    def test_image_over_cap_refuses_before_docker(self):
        for value in OVER_CAP:
            with self.subTest(value=value):
                self.fixture = DockerFixture()
                self.http_calls = []
                error, out, err, calls = self.run_budget_cli(image_max=value)
                self.assert_refuses_before_docker(
                    error, out, err, calls, value, "--image-max-bytes", f"image-{value}"
                )

    # --- binary budgets ----------------------------------------------------

    def test_binary_zero_refuses_before_docker(self):
        error, out, err, calls = self.run_budget_cli(binary_max=0)
        self.assert_refuses_before_docker(
            error, out, err, calls, 0, "--binary-max-bytes", "binary-0"
        )

    def test_binary_negative_refuses_before_docker(self):
        for value in (-1, -100, -2147483648):
            with self.subTest(value=value):
                self.fixture = DockerFixture()
                self.http_calls = []
                error, out, err, calls = self.run_budget_cli(binary_max=value)
                self.assert_refuses_before_docker(
                    error, out, err, calls, value, "--binary-max-bytes", f"binary-{value}"
                )

    def test_binary_over_cap_refuses_before_docker(self):
        for value in OVER_CAP:
            with self.subTest(value=value):
                self.fixture = DockerFixture()
                self.http_calls = []
                error, out, err, calls = self.run_budget_cli(binary_max=value)
                self.assert_refuses_before_docker(
                    error, out, err, calls, value, "--binary-max-bytes", f"binary-{value}"
                )

    def test_both_budgets_invalid_still_refuses_before_docker(self):
        for image_max, binary_max in ((0, 0), (-1, TEN_GIB), (TEN_GIB, -1)):
            with self.subTest(image=image_max, binary=binary_max):
                self.fixture = DockerFixture()
                self.http_calls = []
                error, out, err, calls = self.run_budget_cli(
                    image_max=image_max, binary_max=binary_max
                )
                self.assertIsInstance(error, SystemExit)
                code = str(error.code)
                self.assertTrue(code.startswith("FAIL container smoke:"))
                lowered = code.lower()
                self.assertTrue(
                    "--image-max-bytes" in lowered or "--binary-max-bytes" in lowered,
                    f"refusal names neither flag: {code!r}",
                )
                self.assertEqual(calls, [])

    # --- passing controls --------------------------------------------------

    def test_control_default_budgets_pass_through(self):
        error, out, err, calls = self.run_cli(["container-smoke.py", "two-bot:fixture"])
        self.assert_passes_through(error, out, err, calls, "control-defaults")

    def test_control_explicit_default_budgets_pass_through(self):
        error, out, err, calls = self.run_budget_cli(
            image_max=smoke.IMAGE_MAX_BYTES, binary_max=smoke.BINARY_MAX_BYTES
        )
        self.assert_passes_through(error, out, err, calls, "control-explicit-defaults")

    def test_control_sane_explicit_budgets_pass_through(self):
        error, out, err, calls = self.run_budget_cli(
            image_max=128 * smoke.MIB, binary_max=32 * smoke.MIB
        )
        self.assert_passes_through(error, out, err, calls, "control-sane-explicit")

    def test_control_exact_fixture_budgets_pass_through(self):
        error, out, err, calls = self.run_budget_cli(
            image_max=self.fixture.image_size, binary_max=self.fixture.binary_size
        )
        self.assert_passes_through(error, out, err, calls, "control-exact")

    def test_control_tiny_budgets_reach_comparison_not_refusal(self):
        error, out, err, calls = self.run_budget_cli(image_max=1)
        self.assert_budget_comparison(
            error, out, err, calls, "image exceeds size budget", "control-tiny-image"
        )
        self.assertFalse(any("--detach" in args for args, _ in calls))
        self.fixture = DockerFixture()
        self.http_calls = []
        error, out, err, calls = self.run_budget_cli(binary_max=1)
        self.assert_budget_comparison(
            error, out, err, calls, "release binary exceeds size budget", "control-tiny-binary"
        )


if __name__ == "__main__":
    unittest.main()
