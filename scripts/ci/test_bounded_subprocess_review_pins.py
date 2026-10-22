#!/usr/bin/env python3
"""Review-round pins for the bounded-subprocess bundle (non-protected).

Pins the four review findings on the bounded Git/metadata waits beyond
the protected acceptance suites, which mock ``subprocess.run`` and call
``main()`` directly:

1. The docker-manifests gate must exit nonzero through the real
   ``__main__`` CLI boundary when the locked fetch fails.
2. Each provenance Git timeout must shrink with live monotonic elapsed,
   and an exhausted live budget must refuse before probing.
3. The cargo-metadata child stderr must never reach the gate's stderr.
4. The check-ignore child stderr must never reach the audit's stderr.

Offline only: fake ``cargo``/``git`` binaries exit immediately, clocks
are doubles, and trees are synthetic. No real stall, Cargo compile,
shared pool scan, or CI dispatch.

Run: PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts/ci -p 'test_bounded_subprocess_review_pins.py' -v
"""

import importlib.util
import json
import os
import stat
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

HERE = Path(__file__).resolve().parent
SCRIPTS = HERE.parent
ROOT = SCRIPTS.parent


def _load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


manifests = _load("check_docker_manifests_pins", SCRIPTS / "check-docker-manifests.py")
SPEC = importlib.util.spec_from_file_location(
    "cargo_cache_review_pins", SCRIPTS / "cargo_cache.py")
cache = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(cache)

# Child-stderr sentinel: assembled from fragments so no single literal
# is stored here; nothing under test may echo it. Sentinel assertions
# use fixed messages so a red run never prints the value.
SENTINEL = "Child" + "Stderr" + "NeverEcho" + "9Z"


def _scratch():
    return os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")


def _write_executable(path, text):
    path.write_text(text)
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


class DockerManifestsCliTests(unittest.TestCase):
    """The real CLI boundary must fail closed on a failed locked fetch."""

    def test_cli_exits_nonzero_without_echo_or_pass(self):
        with tempfile.TemporaryDirectory(dir=_scratch()) as tmp:
            fakebin = Path(tmp) / "bin"
            fakebin.mkdir()
            _write_executable(
                fakebin / "cargo",
                "#!/bin/sh\n"
                f'echo "{SENTINEL}" >&2\n'
                "exit 3\n",
            )
            env = dict(os.environ)
            env["PATH"] = str(fakebin) + os.pathsep + env.get("PATH", "")
            env["RUNNER_TEMP"] = tmp
            env["PAPERCLIP_RUN_SCRATCH_DIR"] = tmp
            proc = subprocess.run(
                [sys.executable, str(SCRIPTS / "check-docker-manifests.py")],
                capture_output=True, text=True, env=env, timeout=120,
            )
        self.assertNotEqual(proc.returncode, 0,
                            "the gate CLI must exit nonzero on a failed fetch")
        if SENTINEL in proc.stderr:
            self.fail("the gate CLI must not echo child stderr")
        self.assertIn("docker-manifests", proc.stderr.lower(),
                      "the refusal must name the tool")
        self.assertIn("next:", proc.stderr.lower(),
                      "the refusal must carry a next action")
        self.assertNotIn("PASS", proc.stdout,
                         "a failed fetch must never print a PASS line")
        self.assertNotIn("Traceback", proc.stderr + proc.stdout,
                         "a failed fetch must not leak a traceback")


class DockerManifestsKwargsTests(unittest.TestCase):
    def test_metadata_stderr_is_suppressed(self):
        ok = subprocess.CompletedProcess(("cargo",), 0, "", "")
        with mock.patch.object(manifests.subprocess, "run",
                               return_value=ok) as run:
            manifests.run_locked_fetch(Path(tempfile.gettempdir()))
        _, kwargs = run.call_args
        self.assertIs(kwargs.get("stderr"), subprocess.DEVNULL,
                      "metadata child stderr must stay suppressed")


class ReviewAuditFixture(unittest.TestCase):
    """Synthetic worktree + inventory that reaches the provenance Git calls."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(dir=_scratch())
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.worktrees = self.root / "worktrees"
        self.worktrees.mkdir()
        self.workspace = self.worktrees / "ws"
        (self.workspace / "target" / "debug").mkdir(parents=True)
        self.now = 1_700_000_000.0
        self.max_age = 60
        self.inventory = {
            "version": 1,
            "complete": True,
            "process_scope": "host",
            "captured_at_unix": self.now,
            "workspaces": [{
                "path": str(self.workspace),
                "live_run": False,
                "referenced": False,
                "issue_id": "TEST-1",
                "status": "done",
                "target_provenance": "build_output_only",
            }],
        }
        references = mock.patch.object(cache, "process_references",
                                       return_value=([], []))
        references.start()
        self.addCleanup(references.stop)

    def success_effect(self):
        workspace = self.workspace

        def effect(argv, **kwargs):
            if argv[3:4] == ["rev-parse"]:
                return subprocess.CompletedProcess(
                    ("git",), 0, stdout=f"{workspace}\n", stderr="")
            if argv[3:4] == ["ls-files"]:
                return subprocess.CompletedProcess(
                    ("git",), 0, stdout=b"", stderr=b"")
            if argv[3:4] == ["check-ignore"]:
                return subprocess.CompletedProcess(
                    ("git",), 0, stdout=b"", stderr=b"")
            raise AssertionError("unexpected provenance argv")

        return effect


class ProvenanceLiveBudgetTests(ReviewAuditFixture):
    """Timeouts must track live remaining freshness, not the static age."""

    def _run_with_advancing_clock(self, age, step):
        inventory = dict(self.inventory)
        inventory["captured_at_unix"] = self.now - age
        calls = []
        clock = {"now": 1000.0}
        started = [None]
        inner = self.success_effect()

        def effect(argv, **kwargs):
            calls.append((argv, kwargs))
            clock["now"] += step
            return inner(argv, **kwargs)

        def monotonic():
            if started[0] is None:
                started[0] = clock["now"]
            return clock["now"]

        with mock.patch.object(cache.subprocess, "run", side_effect=effect), \
                mock.patch.object(time, "monotonic", side_effect=monotonic):
            try:
                result = cache.retention_audit(
                    self.worktrees, inventory, proc_root=str(self.root),
                    now=self.now, max_age=self.max_age)
            except cache.Refusal as refusal:
                return calls, ("refused", str(refusal))
        return calls, ("audited", result)

    def test_timeouts_shrink_as_the_clock_advances(self):
        calls, outcome = self._run_with_advancing_clock(age=50, step=3)
        kind, _ = outcome
        self.assertEqual(kind, "audited", "a fitting budget must still audit")
        self.assertEqual(len(calls), 3, "all three calls must run")
        timeouts = [kwargs["timeout"] for _, kwargs in calls]
        self.assertEqual(timeouts, [10.0, 7.0, 4.0],
                         "each call must see the live remaining budget")
        self.assertEqual([argv[3] for argv, _ in calls],
                         ["rev-parse", "ls-files", "check-ignore"],
                         "provenance argv must be preserved in order")

    def test_exhausted_live_budget_refuses_before_probing(self):
        calls, outcome = self._run_with_advancing_clock(age=50, step=9)
        kind, message = outcome
        self.assertEqual(kind, "refused",
                         "an exhausted live budget must refuse, not hang")
        self.assertIn("new inventory", message.lower(),
                      "the refusal must name the next action")
        self.assertEqual(len(calls), 2,
                         "the third call must never probe a dead budget")
        self.assertEqual([kwargs["timeout"] for _, kwargs in calls],
                         [10.0, 1.0],
                         "earlier calls must see the shrinking budget")


class CheckIgnoreCaptureTests(ReviewAuditFixture):
    def test_check_ignore_captures_output_and_stays_unchecked(self):
        calls = []
        inner = self.success_effect()

        def recording(argv, **kwargs):
            calls.append((argv, kwargs))
            return inner(argv, **kwargs)

        with mock.patch.object(cache.subprocess, "run", side_effect=recording):
            cache.retention_audit(
                self.worktrees, dict(self.inventory),
                proc_root=str(self.root), now=self.now, max_age=self.max_age)
        self.assertEqual(len(calls), 3, "all three calls must run")
        _, kwargs = calls[2]
        self.assertTrue(kwargs.get("capture_output"),
                        "check-ignore child streams must be captured")
        if "check" in kwargs:
            self.fail("check-ignore must stay unchecked (exit codes are data)")


class CheckIgnoreStderrTests(unittest.TestCase):
    """A failing check-ignore must not leak child stderr to the audit."""

    def test_check_ignore_stderr_never_reaches_audit_stderr(self):
        with tempfile.TemporaryDirectory(dir=_scratch()) as tmp:
            root = Path(tmp).resolve()
            worktrees = root / "worktrees"
            workspace = worktrees / "ws"
            (workspace / "target" / "debug").mkdir(parents=True)
            proc_root = root / "proc"
            proc_root.mkdir()
            inventory = {
                "version": 1,
                "complete": True,
                "process_scope": "host",
                "captured_at_unix": time.time(),
                "workspaces": [{
                    "path": str(workspace),
                    "live_run": False,
                    "referenced": False,
                    "issue_id": "TEST-1",
                    "status": "done",
                    "target_provenance": "build_output_only",
                }],
            }
            inventory_path = root / "inventory.json"
            inventory_path.write_text(json.dumps(inventory))
            fakebin = root / "bin"
            fakebin.mkdir()
            _write_executable(
                fakebin / "git",
                "#!/bin/sh\n"
                'verb="$3"\n'
                'if [ "$verb" = "rev-parse" ]; then echo "$2"; exit 0; fi\n'
                'if [ "$verb" = "ls-files" ]; then exit 0; fi\n'
                'if [ "$verb" = "check-ignore" ]; then '
                f'echo "{SENTINEL}" >&2; exit 128; fi\n'
                "exit 99\n",
            )
            child = (
                "import json, sys\n"
                "from pathlib import Path\n"
                "sys.path.insert(0, 'scripts')\n"
                "import cargo_cache\n"
                "result = cargo_cache.retention_audit(\n"
                "    Path(sys.argv[1]), json.loads(Path(sys.argv[2]).read_text()),\n"
                "    proc_root=sys.argv[3])\n"
                "print(json.dumps(result, sort_keys=True))\n"
            )
            env = dict(os.environ)
            env["PATH"] = str(fakebin) + os.pathsep + env.get("PATH", "")
            proc = subprocess.run(
                [sys.executable, "-c", child, str(worktrees),
                 str(inventory_path), str(proc_root)],
                capture_output=True, text=True, env=env, timeout=120,
                cwd=str(ROOT),
            )
        self.assertEqual(proc.returncode, 0,
                         "an ineligible candidate must audit, not crash")
        if SENTINEL in proc.stderr:
            self.fail("the audit must not echo check-ignore stderr")
        self.assertIn("git-ignore", proc.stdout,
                      "exit 128 must keep the git-ignore reason")
        self.assertNotIn("Traceback", proc.stderr,
                         "the audit must not leak a traceback")


if __name__ == "__main__":
    unittest.main()
