#!/usr/bin/env python3
"""Acceptance tests: container smoke fails visibly when cleanup fails.

Protected acceptance file: the implementation change must satisfy these
tests without editing them. Test defects are corrected here, not by
weakening assertions in the build.

Contract under test (``scripts/container-smoke.py`` owns three
invocation-generated containers: the never-started measurement
container, the detached main smoke container, and the no-network
health-probe container named ``<main>-probe``):

* A failed ``docker rm --force`` of any one of the three owned
  containers prevents overall success, even when every smoke check
  passed. The failure is reported with a bounded diagnostic that
  names the cleanup stage (measurement, health-probe, or main; the
  generated container name also names the stage).
* Removal timeout and process-launch failure (``OSError`` from
  starting Docker) get the same bounded cleanup diagnostic instead
  of propagating as a raw exception.
* Cleanup diagnostics use fixed vocabulary only: they never print
  uncontrolled Docker stdout/stderr bodies or interpolated
  exception text, and any single diagnostic line stays bounded
  (at most 300 characters in these tests).
* When the smoke already failed, the primary failure stays visible
  (raised, chained, or printed) and the cleanup failure is reported
  separately; cleanup of every applicable owned container is still
  attempted. Containers never created are not "applicable".
* Successful cleanup keeps the current success behavior.
* Only the generated names this invocation created are removed,
  each as exactly ``rm --force <name>``; no prune or other broad
  host operation is issued.
* The cleanup-failure exit contract is documented in
  ``scripts/container-smoke.py`` itself, and at the CLI boundary a
  cleanup failure surfaces as ``FAIL container smoke: ...`` via
  ``SystemExit``, like every other smoke failure.

Docker is fully mocked through the ``DockerFixture`` in
``scripts/test_container_smoke.py``: no daemon, no containers.

Run: PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_container_smoke_cleanup_acceptance.py' -v
"""

import contextlib
import importlib.util
import io
import os
from pathlib import Path
import re
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

ROOT = Path(__file__).resolve().parents[1]

# Assembled from fragments so no credential-shaped literal is stored;
# fixed assertion messages keep a red run from echoing it either.
BODY_SENTINEL = "DockerBody" + "NeverEcho" + "7Q"
STDERR_SENTINEL = "DockerStderr" + "NeverEcho" + "8R"
LAUNCH_SENTINEL = "LaunchDetail" + "NeverEcho" + "9S"
SENTINELS = (BODY_SENTINEL, STDERR_SENTINEL, LAUNCH_SENTINEL)

MAX_DIAGNOSTIC_LINE = 300

STAGE_TOKENS = {
    "measurement": ("measure",),
    "health-probe": ("health-probe", "health probe", "probe"),
    "main": ("main",),
}


def stage_of(name):
    if name.startswith("two-bot-measure-"):
        return "measurement"
    if name.endswith("-probe"):
        return "health-probe"
    if name.startswith("two-bot-smoke-"):
        return "main"
    return None


class CleanupFixture:
    """DockerFixture plus injectable ``rm --force`` failures per stage.

    ``failures`` maps a stage to "nonzero", "timeout", or "launch".
    Every removal attempt is recorded on the wrapped fixture, exactly
    as a real ``docker`` wrapper with ``check=False`` would observe it.
    """

    def __init__(self, failures=None):
        self.inner = DockerFixture()
        self.failures = dict(failures or {})

    def __call__(self, *args, **kwargs):
        if args[:2] == ("rm", "--force") and len(args) >= 3:
            mode = self.failures.get(stage_of(args[2]))
            if mode is not None:
                self.inner.calls.append((args, kwargs))
                if mode == "nonzero":
                    return subprocess.CompletedProcess(
                        args, 1, BODY_SENTINEL + "\n", STDERR_SENTINEL + "\n"
                    )
                if mode == "timeout":
                    raise subprocess.TimeoutExpired(
                        ["docker", *args],
                        kwargs.get("timeout", 30),
                        output=BODY_SENTINEL,
                        stderr=STDERR_SENTINEL,
                    )
                if mode == "launch":
                    raise OSError(f"cannot start docker: {LAUNCH_SENTINEL}")
                raise AssertionError(f"unknown cleanup failure mode: {mode}")
        return self.inner(*args, **kwargs)

    @property
    def calls(self):
        return self.inner.calls

    def removals(self):
        return self.inner.removals()

    def created_names(self):
        names = []
        for args, _ in self.calls:
            if args[0] in ("create", "run") and "--name" in args:
                names.append(args[args.index("--name") + 1])
        return names

    def removed_names(self):
        return [args[2] for args in self.removals() if len(args) >= 3]


def exception_chain_text(error):
    """Bounded chain walk with traceback semantics.

    A chained primary counts as preserved; a context suppressed
    with ``from None`` is not displayed by a traceback, so it counts
    neither as preserved nor as echoed.
    """
    parts = []
    seen = set()
    current = error
    while current is not None and id(current) not in seen and len(parts) < 8:
        seen.add(id(current))
        parts.append(str(current))
        if current.__cause__ is not None:
            current = current.__cause__
        elif current.__suppress_context__:
            current = None
        else:
            current = current.__context__
    return "\n".join(parts)


class CleanupAcceptanceTests(unittest.TestCase):
    def setUp(self):
        self.fixture = CleanupFixture()
        self.clock = 0
        self.http = (
            lambda url: (503, parked_readyz_body())
            if url.endswith("/readyz")
            else (200, {"status": "ok"})
        )

    def tick(self):
        self.clock += 1
        return self.clock

    def run_smoke(self, via_main=False):
        """Run the smoke and capture (exception, stdout, stderr)."""
        error = None
        with patch.object(smoke, "docker", self.fixture), patch.object(
            smoke, "http_response", side_effect=lambda url: self.http(url)
        ), patch.object(smoke.time, "monotonic", side_effect=self.tick), patch.object(
            smoke.time, "sleep"
        ), patch.dict(
            os.environ, {"GITHUB_STEP_SUMMARY": ""}
        ), contextlib.redirect_stdout(
            io.StringIO()
        ) as out, contextlib.redirect_stderr(
            io.StringIO()
        ) as err:
            try:
                if via_main:
                    with patch.object(
                        sys, "argv", ["container-smoke.py", "two-bot:fixture"]
                    ):
                        smoke.main()
                else:
                    smoke.smoke("two-bot:fixture")
            except BaseException as caught:  # SystemExit included on purpose
                error = caught
        return error, out.getvalue(), err.getvalue()

    # --- helpers --------------------------------------------------------

    def combined(self, error, out, err):
        return "\n".join([out, err, exception_chain_text(error)])

    def assert_no_sentinel(self, text, label):
        for sentinel in SENTINELS:
            self.assertNotIn(
                sentinel, text, f"uncontrolled Docker text echoed in case {label}"
            )

    def assert_cleanup_failure(self, error, out, err, stage, label):
        self.assertIsNotNone(
            error, f"overall success must be prevented in case {label}"
        )
        text = self.combined(error, out, err)
        lowered = text.lower()
        self.assertIn("cleanup", lowered, f"no cleanup diagnostic in case {label}")
        self.assertTrue(
            any(token in lowered for token in STAGE_TOKENS[stage]),
            f"cleanup diagnostic does not name stage {stage} in case {label}",
        )
        for line in text.splitlines():
            if "cleanup" in line.lower():
                self.assertLessEqual(
                    len(line),
                    MAX_DIAGNOSTIC_LINE,
                    f"cleanup diagnostic is unbounded in case {label}",
                )
        self.assert_no_sentinel(text, label)

    def assert_only_owned_removals(self, label):
        created = self.fixture.created_names()
        for args in self.fixture.removals():
            self.assertEqual(
                len(args), 3, f"removal is not exactly rm --force <name> in case {label}"
            )
            self.assertIn(
                args[2], created, f"removal targets a foreign container in case {label}"
            )
        flat = [arg for args, _ in self.fixture.calls for arg in args]
        self.assertNotIn("prune", flat, f"broad prune issued in case {label}")

    # --- nonzero removal, one stage at a time ---------------------------

    def test_measurement_cleanup_nonzero_fails_visibly(self):
        self.fixture.failures["measurement"] = "nonzero"
        error, out, err = self.run_smoke()
        self.assert_cleanup_failure(error, out, err, "measurement", "measure-nonzero")
        self.assert_only_owned_removals("measure-nonzero")

    def test_health_probe_cleanup_nonzero_fails_visibly(self):
        self.fixture.failures["health-probe"] = "nonzero"
        error, out, err = self.run_smoke()
        self.assert_cleanup_failure(error, out, err, "health-probe", "probe-nonzero")
        # The main container exists by now: it must still be removed.
        stages = {stage_of(name) for name in self.fixture.removed_names()}
        self.assertEqual(
            stages,
            {"measurement", "health-probe", "main"},
            "all owned containers must be attempted in case probe-nonzero",
        )
        self.assert_only_owned_removals("probe-nonzero")

    def test_main_cleanup_nonzero_fails_visibly(self):
        self.fixture.failures["main"] = "nonzero"
        error, out, err = self.run_smoke()
        self.assert_cleanup_failure(error, out, err, "main", "main-nonzero")
        self.assert_only_owned_removals("main-nonzero")

    # --- timeout and process-launch failure ------------------------------

    def test_health_probe_cleanup_timeout_fails_visibly(self):
        self.fixture.failures["health-probe"] = "timeout"
        error, out, err = self.run_smoke()
        self.assert_cleanup_failure(error, out, err, "health-probe", "probe-timeout")
        stages = {stage_of(name) for name in self.fixture.removed_names()}
        self.assertEqual(
            stages,
            {"measurement", "health-probe", "main"},
            "all owned containers must be attempted in case probe-timeout",
        )
        self.assert_only_owned_removals("probe-timeout")

    def test_main_cleanup_launch_failure_fails_visibly(self):
        self.fixture.failures["main"] = "launch"
        error, out, err = self.run_smoke()
        self.assert_cleanup_failure(error, out, err, "main", "main-launch")
        self.assert_only_owned_removals("main-launch")

    def test_measurement_cleanup_timeout_fails_visibly(self):
        self.fixture.failures["measurement"] = "timeout"
        error, out, err = self.run_smoke()
        self.assert_cleanup_failure(error, out, err, "measurement", "measure-timeout")
        self.assert_only_owned_removals("measure-timeout")

    # --- CLI exit contract -----------------------------------------------

    def test_main_cleanup_failure_uses_fail_container_smoke_exit(self):
        self.fixture.failures["main"] = "nonzero"
        error, out, err = self.run_smoke(via_main=True)
        self.assertIsInstance(
            error, SystemExit, "cleanup failure must exit via SystemExit in CLI case"
        )
        code = str(error.code)
        self.assertTrue(
            code.startswith("FAIL container smoke:"),
            "CLI cleanup failure must use the FAIL container smoke contract",
        )
        lowered = (code + "\n" + out + "\n" + err).lower()
        self.assertIn("cleanup", lowered, "no cleanup diagnostic in CLI case")
        self.assertIn("main", lowered, "CLI diagnostic does not name the main stage")
        self.assertLessEqual(
            len(code), MAX_DIAGNOSTIC_LINE, "CLI failure line is unbounded"
        )
        self.assert_no_sentinel(code + out + err, "cli-main-nonzero")

    # --- primary failure plus cleanup failure ------------------------------

    def test_primary_health_failure_and_main_cleanup_failure_both_reported(self):
        self.http = lambda url: (500, {})
        self.fixture.failures["main"] = "nonzero"
        error, out, err = self.run_smoke()
        self.assert_cleanup_failure(error, out, err, "main", "primary-health-main")
        text = self.combined(error, out, err)
        self.assertIn(
            "/health did not return 200",
            text,
            "primary failure not preserved in case primary-health-main",
        )
        # Probe was never created: only measurement and main are applicable.
        self.assertEqual(
            {stage_of(name) for name in self.fixture.removed_names()},
            {"measurement", "main"},
            "applicable cleanups not all attempted in case primary-health-main",
        )
        self.assert_only_owned_removals("primary-health-main")

    def test_primary_probe_timeout_and_probe_cleanup_failure_both_reported(self):
        self.fixture.inner.probe_timeout = True
        self.fixture.failures["health-probe"] = "nonzero"
        error, out, err = self.run_smoke()
        self.assert_cleanup_failure(error, out, err, "health-probe", "primary-probe")
        text = self.combined(error, out, err).lower()
        self.assertTrue(
            "timed out" in text or "timeout" in text,
            "primary timeout not preserved in case primary-probe",
        )
        self.assertEqual(
            {stage_of(name) for name in self.fixture.removed_names()},
            {"measurement", "health-probe", "main"},
            "applicable cleanups not all attempted in case primary-probe",
        )
        self.assert_only_owned_removals("primary-probe")

    def test_primary_measure_timeout_and_measure_cleanup_failure_both_reported(self):
        self.fixture.inner.measure_timeout = True
        self.fixture.failures["measurement"] = "nonzero"
        error, out, err = self.run_smoke()
        self.assert_cleanup_failure(error, out, err, "measurement", "primary-measure")
        text = self.combined(error, out, err).lower()
        self.assertTrue(
            "timed out" in text or "timeout" in text,
            "primary timeout not preserved in case primary-measure",
        )
        self.assertEqual(
            self.fixture.removed_names(),
            self.fixture.created_names(),
            "the one created container must be (attempted) removed in case primary-measure",
        )
        self.assert_only_owned_removals("primary-measure")

    # --- documentation ------------------------------------------------------

    def test_cleanup_failure_exit_contract_is_documented_in_the_script(self):
        source = (ROOT / "scripts" / "container-smoke.py").read_text(encoding="utf-8")
        self.assertTrue(
            re.search(
                r"cleanup[^.\n]{0,160}?(fail|exit|nonzero)",
                source,
                flags=re.IGNORECASE | re.DOTALL,
            ),
            "container-smoke.py must document the cleanup-failure exit contract",
        )

    # --- passing controls -----------------------------------------------------

    def test_control_successful_cleanup_retains_success(self):
        error, out, err = self.run_smoke()
        self.assertIsNone(error, f"valid smoke must pass, got: {error!r}")
        self.assertIn("SIGTERM exits 0", out)
        self.assertEqual(
            {stage_of(name) for name in self.fixture.removed_names()},
            {"measurement", "health-probe", "main"},
        )
        self.assertEqual(
            sorted(self.fixture.removed_names()),
            sorted(self.fixture.created_names()),
        )
        self.assert_only_owned_removals("control-success")

    def test_control_primary_failure_without_cleanup_failure_stays_primary(self):
        self.http = lambda url: (500, {})
        error, out, err = self.run_smoke()
        self.assertIsNotNone(error)
        self.assertIn("/health did not return 200", self.combined(error, out, err))
        self.assertEqual(
            {stage_of(name) for name in self.fixture.removed_names()},
            {"measurement", "main"},
        )
        self.assert_only_owned_removals("control-primary-only")


if __name__ == "__main__":
    unittest.main()
