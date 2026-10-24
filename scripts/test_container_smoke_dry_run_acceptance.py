#!/usr/bin/env python3
"""Acceptance tests: preview container-smoke mutations with --dry-run.

Protected acceptance file: the implementation change must satisfy these
tests without editing them. Test defects are corrected here, not by
weakening assertions in the build.

Contract under test (``scripts/container-smoke.py`` creates, runs, execs
into, kills and removes containers but offers no preview):

* ``python3 scripts/container-smoke.py IMAGE --dry-run`` exits 0 and prints
  a stable plan to stdout naming every mutation it would perform: the
  measurement container, the detached main run with its publish flag and
  memory cap, the no-network health-probe run, the SIGTERM shutdown and
  the owned-container removals, plus the image under test.
* The plan is stable: two invocations print byte-identical stdout (no
  random container names, ports or timestamps).
* Zero mutations: no ``docker`` invocation of any kind, no loopback HTTP,
  no subprocess at all.
* A dry run never enforces size budgets (nothing is measured) and never
  needs a Docker daemon.
* Well-formed live behavior passes through byte-identically (controls).

Docker, subprocess and HTTP are fully mocked: no daemon, no containers,
no sockets.

Run: PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_container_smoke_dry_run_acceptance.py' -v
"""

import contextlib
import importlib.util
import io
import os
import subprocess
import sys
import unittest
from pathlib import Path
from unittest.mock import patch

_spec = importlib.util.spec_from_file_location(
    "container_smoke_dry_run_acceptance_target",
    Path(__file__).with_name("container-smoke.py"),
)
smoke = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(smoke)

IMAGE = "dry-run-plan.invalid/demo:acceptance"


def run_main(argv):
    """Call smoke.main() with patched argv; return (code, stdout, stderr).

    main() returns None on success and raises SystemExit on failure; the
    environment is cleared so GITHUB_STEP_SUMMARY file writes cannot fire.
    """
    stdout, stderr = io.StringIO(), io.StringIO()
    with (
        patch.dict(os.environ, {}, clear=True),
        patch.object(sys, "argv", argv),
        contextlib.redirect_stdout(stdout),
        contextlib.redirect_stderr(stderr),
    ):
        try:
            result = smoke.main()
        except SystemExit as exited:
            code = exited.code if exited.code is not None else 0
            return code, stdout.getvalue(), stderr.getvalue()
    if result in (None, 0):
        return 0, stdout.getvalue(), stderr.getvalue()
    return result, stdout.getvalue(), stderr.getvalue()


@contextlib.contextmanager
def no_mutations():
    """Refuse every docker/subprocess/HTTP call; yield the (empty) call log."""
    calls = []

    def refuse_docker(*args, **kwargs):
        calls.append(("docker", args))
        raise AssertionError(f"docker called during --dry-run: {args!r}")

    def refuse_subprocess(*args, **kwargs):
        calls.append(("subprocess", args))
        raise AssertionError(f"subprocess called during --dry-run: {args!r}")

    def refuse_http(*args, **kwargs):
        calls.append(("http", args))
        raise AssertionError("HTTP called during --dry-run")

    with (
        patch.object(smoke, "docker", side_effect=refuse_docker),
        patch("subprocess.run", side_effect=refuse_subprocess),
        patch("http.client.HTTPConnection", side_effect=refuse_http),
    ):
        yield calls


class DryRunPlanTests(unittest.TestCase):
    def test_dry_run_exits_zero_and_names_every_mutation_step(self):
        with no_mutations() as calls:
            code, out, err = run_main(["container-smoke.py", IMAGE, "--dry-run"])
        self.assertEqual(
            code, 0,
            f"--dry-run must exit 0 with zero mutations; got {code}. "
            f"stderr tail: {err[-500:]!r}",
        )
        self.assertEqual(calls, [], "dry-run must not call docker/subprocess/HTTP")
        lowered = out.lower().replace("_", "-")
        self.assertIn("dry-run", lowered, "plan must say it is a dry run")
        self.assertIn("plan", lowered, "plan must say it is a plan")
        self.assertIn(IMAGE, out, "plan must name the image under test")
        for token in ("measure", "publish", "127.0.0.1", "8080", "256m",
                      "probe", "term", "remov", "health", "readyz"):
            self.assertIn(token, lowered, f"plan must name the {token!r} step")

    def test_dry_run_plan_is_stable_across_runs(self):
        with no_mutations():
            code1, out1, err1 = run_main(["container-smoke.py", IMAGE, "--dry-run"])
            code2, out2, err2 = run_main(["container-smoke.py", IMAGE, "--dry-run"])
        self.assertEqual(code1, 0, f"first dry-run exited {code1}: {err1[-300:]!r}")
        self.assertEqual(code2, 0, f"second dry-run exited {code2}: {err2[-300:]!r}")
        self.assertNotEqual(out1.strip(), "", "plan must print something to stdout")
        self.assertEqual(
            out1, out2,
            "dry-run plan must be byte-identical across runs "
            "(no random container names, ports or timestamps)",
        )

    def test_dry_run_with_tiny_budgets_still_exits_zero(self):
        with no_mutations() as calls:
            code, out, err = run_main(["container-smoke.py", IMAGE, "--dry-run",
                                       "--image-max-bytes", "1", "--binary-max-bytes", "1"])
        self.assertEqual(
            code, 0,
            f"dry-run previews without measuring, so budgets must not fail it; "
            f"got {code}. stderr tail: {err[-500:]!r}",
        )
        self.assertEqual(calls, [], "dry-run must not call docker/subprocess/HTTP")
        self.assertIn(IMAGE, out, "plan must still name the image under test")


class LiveBehaviorControls(unittest.TestCase):
    """Well-formed live behavior passes through byte-identically."""

    def test_unknown_flag_still_rejected(self):
        code, _, _ = run_main(["container-smoke.py", "img", "--bogus-flag-xyz"])
        self.assertNotEqual(code, 0, "unknown flags must still be rejected")

    def test_missing_image_still_rejected(self):
        code, _, _ = run_main(["container-smoke.py"])
        self.assertNotEqual(code, 0, "the image argument must still be required")

    def test_help_still_names_the_image_argument(self):
        code, out, _ = run_main(["container-smoke.py", "--help"])
        self.assertEqual(code, 0, "--help must still exit 0")
        self.assertIn("image", out.lower(), "--help must still document the image")

    def test_live_wiring_still_passes_size_budgets(self):
        seen = {}

        def fake_smoke(image, image_max_bytes=0, binary_max_bytes=0):
            seen["image"] = image
            seen["image_max_bytes"] = image_max_bytes
            seen["binary_max_bytes"] = binary_max_bytes

        with patch.object(smoke, "smoke", side_effect=fake_smoke):
            code, _, err = run_main(["container-smoke.py", "img",
                                     "--image-max-bytes", "123",
                                     "--binary-max-bytes", "456"])
        self.assertEqual(code, 0, f"live wiring must still exit 0: {err[-300:]!r}")
        self.assertEqual(seen, {"image": "img",
                                "image_max_bytes": 123, "binary_max_bytes": 456})

    def test_module_constants_unchanged(self):
        self.assertEqual(smoke.MIB, 1024 * 1024)
        self.assertEqual(smoke.IMAGE_MAX_BYTES, 112 * 1024 * 1024)
        self.assertEqual(smoke.BINARY_MAX_BYTES, 18 * 1024 * 1024)
        self.assertEqual(smoke.MAX_BODY_BYTES, 64 * 1024)
        self.assertEqual(smoke.BINARY, "/home/nonroot/two-bot")
        self.assertEqual(smoke.CA_BUNDLE, "/etc/ssl/certs/ca-certificates.crt")
        self.assertEqual(smoke.USER_AGENT, "two-bot-next-container-smoke/1.0 (loopback)")

    def test_require_semantics_unchanged(self):
        smoke.require(True, "must not raise")
        with self.assertRaises(RuntimeError) as caught:
            smoke.require(False, "control message")
        self.assertEqual(str(caught.exception), "control message")

    def test_owned_container_still_removes_exactly_its_own_name(self):
        calls = []

        def fake_docker(*args, **kwargs):
            calls.append(args)
            return subprocess.CompletedProcess(args, 0, "", "")

        with patch.object(smoke, "docker", side_effect=fake_docker):
            with smoke.owned_container("two-bot-smoke-control", "main"):
                pass
        self.assertEqual(calls, [("rm", "--force", "two-bot-smoke-control")])


if __name__ == "__main__":
    unittest.main()
