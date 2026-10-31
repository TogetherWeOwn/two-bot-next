#!/usr/bin/env python3
"""Acceptance tests: bound Git/metadata subprocesses in CI selectors and gates.

CI/selector half of the bounded-subprocess bundle: eight ``subprocess.run``
calls without ``timeout=`` across seven tools must gain explicit finite
timeouts under one small documented local policy, and every timeout,
spawn/OS failure and nonzero child result must map into each tool's
existing fail-closed vocabulary with a stable code and a next action --
never a traceback, never echoed child stderr/remote content, and never an
empty diff that silently skips required lanes.

Call sites under test
---------------------
1. ``scripts/check-docker-manifests.py`` ``run_locked_fetch`` (cargo metadata)
2. ``scripts/check-migrations.py`` ``git`` (baseline reads)
3. ``scripts/check_parity_baseline.py`` ``git`` (ancestry/link proofs)
4. ``scripts/ci/check-floor-guard.py`` ``_git`` (diff collection)
5. ``scripts/ci/check-squawk-migrations.py`` ``git`` (changed-path diff)
6. ``scripts/ci/check-squawk-migrations.py`` ``changed_paths`` rev-parse
7. ``scripts/container-inputs.py`` ``changed_files`` (image selector)
8. ``scripts/job-inputs.py`` ``git_diff_names`` (job selector)

Locked contract
---------------
- Every call passes ``timeout=`` with a finite value in the envelope
  ``1..120`` seconds. The Builder picks the exact value(s) and documents
  them as the local policy; within one tool every Git call uses the same
  value (one policy, not per-call improvisation). ``argv``, capture mode,
  ``check`` mode and offline/locked metadata flags are preserved exactly.
- ``TimeoutExpired`` maps to the tool's fail-closed path with a message
  naming the step, the word ``timeout``, and a ``next:`` action.
- ``OSError`` (spawn failure) maps the same way with ``spawn`` or
  ``unavailable`` in place of ``timeout``.
- Nonzero child results map the same way and keep the tool's existing
  refusal prefix and exit code; the message carries the exit status and a
  ``next:`` action instead of the child's stderr.
- No failure path prints a Python traceback or echoes child stderr/remote
  content (pinned by a sentinel below; assertion messages are fixed so a
  red run never echoes the sentinel into the test log).
- Selector failures (container/job inputs) exit 2 and emit no lane
  selection line; they never return an empty change list. Gate failures
  keep their existing nonzero exits and never print a PASS/OK/clean line.
- RED is behavioral: every failing assertion fails on a timeout kwarg,
  exit code, refusal text or output comparison against live code paths,
  never on an import error, missing fixture or refusal to run.

Protected file: the Builder builds on this commit and must never edit
this file; test corrections go back to its author.

Run: PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts/ci -p 'test_bounded_subprocess_ci_acceptance.py' -v
"""

import contextlib
import importlib.util
import io
import math
import os
import re
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

HERE = Path(__file__).resolve().parent
SCRIPTS = HERE.parent


def _load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


manifests = _load("check_docker_manifests", SCRIPTS / "check-docker-manifests.py")
migrations = _load("check_migrations", SCRIPTS / "check-migrations.py")
parity = _load("check_parity_baseline", SCRIPTS / "check_parity_baseline.py")
floor = _load("floor_guard", HERE / "check-floor-guard.py")
squawk = _load("squawk_gate", HERE / "check-squawk-migrations.py")
container = _load("container_inputs", SCRIPTS / "container-inputs.py")
job = _load("job_inputs", SCRIPTS / "job-inputs.py")

# Envelope for the Builder's documented local policy value(s).
TIMEOUT_MIN = 1
TIMEOUT_MAX = 120

# Child-stderr sentinel: assembled from fragments so no remote-content
# literal is stored here; nothing under test may echo it.
SENTINEL = "Child" + "Stderr" + "NeverEcho" + "9Z"

RAW_SUBPROCESS_ERRORS = (
    subprocess.TimeoutExpired,
    subprocess.CalledProcessError,
    OSError,
)


class TimeoutPolicyAssertions:
    """Shared envelope checks with fixed failure messages."""

    def assert_valid_timeout(self, kwargs, label):
        self.assertIn("timeout", kwargs, f"missing timeout kwarg ({label})")
        value = kwargs["timeout"]
        self.assertIsInstance(value, (int, float),
                              f"timeout must be numeric ({label})")
        self.assertNotIsInstance(value, bool, f"timeout must be numeric ({label})")
        self.assertTrue(math.isfinite(value), f"timeout must be finite ({label})")
        self.assertGreaterEqual(value, TIMEOUT_MIN,
                                f"timeout below policy floor ({label})")
        self.assertLessEqual(value, TIMEOUT_MAX,
                             f"timeout above policy ceiling ({label})")
        return value

    def assert_no_echo(self, text, label):
        self.assertNotIn(SENTINEL, text, f"echoed child stderr ({label})")

    def assert_no_traceback(self, text, label):
        self.assertNotIn("Traceback", text, f"leaked a traceback ({label})")

    def assert_next_action(self, text, label):
        self.assertIn("next:", text.lower(),
                      f"refusal must carry a next action ({label})")


def _ok_bytes(*, stdout=b"", stderr=b"", args=("git",)):
    return subprocess.CompletedProcess(args, 0, stdout, stderr)


def _ok_text(*, stdout="", stderr="", args=("git",)):
    return subprocess.CompletedProcess(args, 0, stdout, stderr)


def _mock_kwargs(effect):
    """Route exceptions/sequences to side_effect, plain results to return_value."""
    if isinstance(effect, BaseException) or isinstance(effect, (list, tuple)):
        return {"side_effect": effect}
    return {"return_value": effect}


def _fail_bytes(status=128, stdout=b"", stderr=b""):
    return subprocess.CompletedProcess(("git",), status, stdout, stderr)


class ManifestArgvTests(TimeoutPolicyAssertions, unittest.TestCase):
    """ Cargo-metadata argv is preserved exactly (positive control)."""

    def test_argv_capture_and_check_modes_unchanged(self):
        dest = Path(tempfile.gettempdir()) / "manifest-fixture"
        with mock.patch.object(manifests.subprocess, "run",
                               return_value=_ok_text(args=("cargo",))) as run:
            manifests.run_locked_fetch(dest)
        (argv,), kwargs = run.call_args
        self.assertEqual(
            argv,
            ["cargo", "metadata", "--offline", "--locked", "--no-deps",
             "--format-version", "1", "--manifest-path",
             str(dest / "Cargo.toml")],
            "metadata argv must stay offline and locked",
        )
        self.assertTrue(kwargs.get("check"), "metadata must keep check=True")
        self.assertEqual(kwargs.get("stdout"), subprocess.DEVNULL,
                         "metadata stdout must stay suppressed")

    def test_valid_fetch_passes_through_silently(self):
        with mock.patch.object(manifests.subprocess, "run",
                               return_value=_ok_text(args=("cargo",))):
            manifests.run_locked_fetch(Path(tempfile.gettempdir()))


class ManifestTimeoutTests(TimeoutPolicyAssertions, unittest.TestCase):
    def test_metadata_call_is_bounded(self):
        with mock.patch.object(manifests.subprocess, "run",
                               return_value=_ok_text(args=("cargo",))) as run:
            manifests.run_locked_fetch(Path(tempfile.gettempdir()))
        _, kwargs = run.call_args
        self.assert_valid_timeout(kwargs, "cargo metadata")

    def _mapped_message(self, effect):
        with mock.patch.object(manifests.subprocess, "run", **_mock_kwargs(effect)):
            try:
                manifests.run_locked_fetch(Path(tempfile.gettempdir()))
            except RAW_SUBPROCESS_ERRORS as error:
                self.fail(f"must map raw {type(error).__name__} to the refusal vocabulary")
            except (AssertionError, ValueError) as error:
                return str(error)
        self.fail("a failed metadata fetch must refuse, never pass silently")

    def test_timeout_maps_to_refusal(self):
        message = self._mapped_message(
            subprocess.TimeoutExpired("cargo", 30))
        self.assertIn("metadata", message.lower(), "refusal must name the step")
        self.assertIn("timeout", message.lower(), "refusal must name the class")
        self.assert_next_action(message, "metadata timeout")

    def test_spawn_failure_maps_to_refusal(self):
        message = self._mapped_message(OSError(2, "missing binary"))
        self.assertIn("metadata", message.lower(), "refusal must name the step")
        self.assertTrue("spawn" in message.lower() or "unavailable" in message.lower(),
                        "refusal must name the class")
        self.assert_next_action(message, "metadata spawn failure")

    def test_nonzero_maps_to_refusal_without_echo(self):
        message = self._mapped_message(
            subprocess.CalledProcessError(101, "cargo", stderr=SENTINEL))
        self.assertIn("metadata", message.lower(), "refusal must name the step")
        self.assert_next_action(message, "metadata nonzero")
        self.assert_no_echo(message, "metadata nonzero")

    def _main_code(self, effect):
        out, err = io.StringIO(), io.StringIO()
        with tempfile.TemporaryDirectory() as scratch, \
                mock.patch.object(manifests.subprocess, "run", **_mock_kwargs(effect)), \
                mock.patch.dict(os.environ, {"RUNNER_TEMP": scratch},
                                clear=False), \
                contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            if "PAPERCLIP_RUN_SCRATCH_DIR" in os.environ:
                with mock.patch.dict(os.environ):
                    del os.environ["PAPERCLIP_RUN_SCRATCH_DIR"]
                    return self._invoke_main(out, err)
            return self._invoke_main(out, err)

    def _invoke_main(self, out, err):
        try:
            result = manifests.main()
        except SystemExit as exit:
            return exit.code, out.getvalue(), err.getvalue()
        except RAW_SUBPROCESS_ERRORS as error:
            self.fail(f"main must map raw {type(error).__name__}, never propagate it")
        self.assertIsNotNone(result, "main must not report success on a failed fetch")
        return result, out.getvalue(), err.getvalue()

    def _assert_main_refuses(self, effect, label, needle):
        code, out, err = self._main_code(effect)
        self.assertNotEqual(code, 0, f"main must exit nonzero ({label})")
        combined = out + err
        self.assertIn("docker-manifests", combined.lower(),
                      f"refusal must name the tool ({label})")
        self.assertIn(needle, combined.lower(),
                      f"refusal must name the class ({label})")
        self.assert_next_action(combined, label)
        self.assert_no_echo(combined, label)
        self.assert_no_traceback(combined, label)

    def test_main_timeout_refuses_cleanly(self):
        self._assert_main_refuses(subprocess.TimeoutExpired("cargo", 30),
                                  "main timeout", "timeout")

    def test_main_spawn_failure_refuses_cleanly(self):
        self._assert_main_refuses(OSError(2, "missing binary"),
                                  "main spawn failure", "spawn")

    def test_main_nonzero_refuses_cleanly(self):
        self._assert_main_refuses(
            subprocess.CalledProcessError(101, "cargo", stderr=SENTINEL),
            "main nonzero", "metadata")


class MigrationGitTests(TimeoutPolicyAssertions, unittest.TestCase):
    def test_argv_and_capture_preserved(self):
        with mock.patch.object(migrations.subprocess, "run",
                               return_value=_ok_bytes(stdout=b"deadbee")) as run:
            self.assertEqual(migrations.git(Path("/tmp"), "rev-parse", "x"), b"deadbee")
        (argv,), kwargs = run.call_args
        self.assertEqual(argv[:3], ["git", "-C", "/tmp"],
                         "git argv prefix must be preserved")
        self.assertEqual(argv[3:], ["rev-parse", "x"],
                         "git argv suffix must be preserved")
        self.assertTrue(kwargs.get("capture_output"), "git must stay captured")

    def test_call_is_bounded(self):
        with mock.patch.object(migrations.subprocess, "run",
                               return_value=_ok_bytes()) as run:
            migrations.git(Path("/tmp"), "rev-parse", "x")
        _, kwargs = run.call_args
        self.assert_valid_timeout(kwargs, "migration git")

    def _mapped_message(self, effect):
        with mock.patch.object(migrations.subprocess, "run", **_mock_kwargs(effect)):
            try:
                migrations.git(Path("/tmp"), "rev-parse", "x")
            except migrations.MigrationError as error:
                return str(error)
            except RAW_SUBPROCESS_ERRORS as error:
                self.fail(f"must map raw {type(error).__name__} to MigrationError")
        self.fail("a failed git read must refuse, never pass silently")

    def test_timeout_maps_to_migration_error(self):
        message = self._mapped_message(subprocess.TimeoutExpired("git", 30))
        self.assertIn("Git baseline", message, "must keep the existing prefix")
        self.assertIn("timeout", message.lower(), "must name the class")
        self.assert_next_action(message, "migration timeout")

    def test_spawn_failure_maps_to_migration_error(self):
        message = self._mapped_message(OSError(2, "missing binary"))
        self.assertIn("Git baseline", message, "must keep the existing prefix")
        self.assertTrue("spawn" in message.lower() or "unavailable" in message.lower(),
                        "must name the class")
        self.assert_next_action(message, "migration spawn failure")

    def test_nonzero_keeps_prefix_without_echo(self):
        message = self._mapped_message(
            _fail_bytes(stderr=SENTINEL.encode()))
        self.assertIn("Git baseline", message, "must keep the existing prefix")
        self.assert_next_action(message, "migration nonzero")
        self.assert_no_echo(message, "migration nonzero")

    def _run_main(self, effect):
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch)
            target = root / "crates" / "a" / "migrations"
            target.mkdir(parents=True)
            payload = b"-- initial\n"
            (target / "0001_init.sql").write_bytes(payload)
            digest = migrations.digest(payload)
            (root / "migrations.lock").write_text(
                '{"version": 1, "migrations": '
                f'{{"crates/a/migrations/0001_init.sql": {{"sha256": "{digest}", '
                '"justification": "initial"}}}' )
            out, err = io.StringIO(), io.StringIO()
            with mock.patch.object(migrations.subprocess, "run",
                                    **_mock_kwargs(effect)), \
                    contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                try:
                    code = migrations.main(["--root", str(root),
                                            "--base-ref", "main"])
                except RAW_SUBPROCESS_ERRORS as error:
                    self.fail(f"main must map raw {type(error).__name__}")
            return code, out.getvalue(), err.getvalue()

    def _assert_main_refuses(self, effect, label, needle):
        code, out, err = self._run_main(effect)
        self.assertEqual(code, 1, f"main must exit 1 ({label})")
        combined = out + err
        self.assertIn("Migration check failed:", combined,
                      f"must keep the existing prefix ({label})")
        self.assertIn(needle, combined.lower(), f"must name the class ({label})")
        self.assert_next_action(combined, label)
        self.assert_no_echo(combined, label)
        self.assert_no_traceback(combined, label)
        self.assertNotIn("Migration check OK", combined,
                         f"must never print OK on failure ({label})")

    def test_main_timeout_refuses_cleanly(self):
        self._assert_main_refuses(subprocess.TimeoutExpired("git", 30),
                                  "main timeout", "timeout")

    def test_main_spawn_failure_refuses_cleanly(self):
        self._assert_main_refuses(OSError(2, "missing binary"),
                                  "main spawn failure", "spawn")

    def test_main_nonzero_refuses_cleanly(self):
        self._assert_main_refuses(_fail_bytes(stderr=SENTINEL.encode()),
                                  "main nonzero", "baseline")


class ParityGitTests(TimeoutPolicyAssertions, unittest.TestCase):
    def test_argv_and_modes_preserved(self):
        with mock.patch.object(parity.subprocess, "run",
                               return_value=_ok_text(stdout="abc\n")) as run:
            self.assertEqual(parity.git(Path("/tmp"), "rev-parse", "x"), "abc")
        (argv,), kwargs = run.call_args
        self.assertEqual(argv[:3], ["git", "-C", "/tmp"],
                         "git argv prefix must be preserved")
        self.assertEqual(argv[3:], ["rev-parse", "x"],
                         "git argv suffix must be preserved")
        self.assertTrue(kwargs.get("capture_output"), "git must stay captured")
        self.assertTrue(kwargs.get("text"), "git must stay in text mode")
        self.assertFalse(kwargs.get("check"), "git must stay unchecked")

    def test_call_is_bounded(self):
        with mock.patch.object(parity.subprocess, "run",
                               return_value=_ok_text()) as run:
            parity.git(Path("/tmp"), "rev-parse", "x")
        _, kwargs = run.call_args
        self.assert_valid_timeout(kwargs, "parity git")

    def _mapped_message(self, effect):
        with mock.patch.object(parity.subprocess, "run", **_mock_kwargs(effect)):
            try:
                parity.git(Path("/tmp"), "rev-parse", "x")
            except parity.ParityError as error:
                return str(error)
            except RAW_SUBPROCESS_ERRORS as error:
                self.fail(f"must map raw {type(error).__name__} to ParityError")
        self.fail("a failed git read must refuse, never pass silently")

    def test_timeout_maps_to_parity_error(self):
        message = self._mapped_message(subprocess.TimeoutExpired("git", 30))
        self.assertIn("timeout", message.lower(), "must name the class")
        self.assert_next_action(message, "parity timeout")

    def test_spawn_failure_maps_to_parity_error(self):
        message = self._mapped_message(OSError(2, "missing binary"))
        self.assertTrue("spawn" in message.lower() or "unavailable" in message.lower(),
                        "must name the class")
        self.assert_next_action(message, "parity spawn failure")

    def test_nonzero_refuses_without_echo(self):
        with mock.patch.object(parity.subprocess, "run",
                               return_value=subprocess.CompletedProcess(
                                   ("git",), 128, "", SENTINEL)):
            try:
                parity.git(Path("/tmp"), "rev-parse", "x")
            except parity.ParityError as error:
                message = str(error)
            else:
                self.fail("a nonzero git result must refuse")
        self.assert_next_action(message, "parity nonzero")
        self.assert_no_echo(message, "parity nonzero")

    def _run_main(self, effect):
        with tempfile.TemporaryDirectory() as scratch:
            out, err = io.StringIO(), io.StringIO()
            argv = ["check_parity_baseline.py", "--legacy-repo", scratch]
            with mock.patch.object(parity.subprocess, "run", **_mock_kwargs(effect)), \
                    mock.patch.object(sys, "argv", argv), \
                    contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                try:
                    code = parity.main()
                except RAW_SUBPROCESS_ERRORS as error:
                    self.fail(f"main must map raw {type(error).__name__}")
            return code, out.getvalue(), err.getvalue()

    def _assert_main_refuses(self, effect, label, needle):
        code, out, err = self._run_main(effect)
        self.assertEqual(code, 1, f"main must exit 1 ({label})")
        combined = out + err
        self.assertTrue(combined.startswith("FAIL:"),
                        f"must keep the FAIL prefix ({label})")
        self.assertIn(needle, combined.lower(), f"must name the class ({label})")
        self.assert_next_action(combined, label)
        self.assert_no_echo(combined, label)
        self.assert_no_traceback(combined, label)
        self.assertNotIn("PASS:", combined, f"must never print PASS ({label})")

    def test_main_timeout_refuses_cleanly(self):
        self._assert_main_refuses(subprocess.TimeoutExpired("git", 30),
                                  "main timeout", "timeout")

    def test_main_spawn_failure_refuses_cleanly(self):
        self._assert_main_refuses(OSError(2, "missing binary"),
                                  "main spawn failure", "spawn")

    def test_main_nonzero_refuses_cleanly(self):
        self._assert_main_refuses(
            subprocess.CompletedProcess(("git",), 128, "", SENTINEL),
            "main nonzero", "git")


class FloorGuardGitTests(TimeoutPolicyAssertions, unittest.TestCase):
    def test_argv_and_modes_preserved(self):
        with mock.patch.object(floor.subprocess, "run",
                               return_value=_ok_text(stdout="base\n")) as run:
            result = floor._git(Path("/tmp"), "merge-base", "a", "b")
        self.assertEqual(result.stdout, "base\n")
        (argv,), kwargs = run.call_args
        self.assertEqual(argv, ["git", "merge-base", "a", "b"],
                         "git argv must be preserved")
        self.assertEqual(kwargs.get("cwd"), Path("/tmp"), "cwd must be preserved")
        self.assertTrue(kwargs.get("capture_output"), "must stay captured")
        self.assertTrue(kwargs.get("text"), "must stay in text mode")

    def test_call_is_bounded(self):
        with mock.patch.object(floor.subprocess, "run",
                               return_value=_ok_text()) as run:
            floor._git(Path("/tmp"), "merge-base", "a", "b")
        _, kwargs = run.call_args
        self.assert_valid_timeout(kwargs, "floor git")

    def _assert_mapped(self, effect, label):
        with mock.patch.object(floor.subprocess, "run", **_mock_kwargs(effect)):
            try:
                floor._git(Path("/tmp"), "merge-base", "a", "b")
            except (ValueError, floor.NoMergeBase):
                return
            except RAW_SUBPROCESS_ERRORS as error:
                self.fail(f"must map raw {type(error).__name__} ({label})")
        self.fail(f"a failed git call must refuse ({label})")

    def test_timeout_maps_to_guard_vocabulary(self):
        self._assert_mapped(subprocess.TimeoutExpired("git", 30), "timeout")

    def test_spawn_failure_maps_to_guard_vocabulary(self):
        self._assert_mapped(OSError(2, "missing binary"), "spawn failure")

    def _run_main(self, effect):
        with tempfile.TemporaryDirectory() as scratch:
            out, err = io.StringIO(), io.StringIO()
            with mock.patch.object(floor.subprocess, "run", **_mock_kwargs(effect)), \
                    contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                try:
                    floor.main(["--root", scratch, "--base-ref", "origin/main"])
                except SystemExit as exit:
                    return exit.code, out.getvalue(), err.getvalue()
                except RAW_SUBPROCESS_ERRORS as error:
                    self.fail(f"main must map raw {type(error).__name__}")
        self.fail("main must exit, never return, on a failed git call")

    def _assert_main_exits_2(self, effect, label, needle):
        code, out, err = self._run_main(effect)
        self.assertEqual(code, 2, f"main must exit 2 ({label})")
        combined = out + err
        self.assertIn("floor-guard:", combined, f"must keep the prefix ({label})")
        self.assertIn(needle, combined.lower(), f"must name the class ({label})")
        self.assert_next_action(combined, label)
        self.assert_no_traceback(combined, label)
        self.assertNotIn("floor-guard: clean", combined,
                         f"must never print clean ({label})")

    def test_main_timeout_exits_2(self):
        self._assert_main_exits_2(subprocess.TimeoutExpired("git", 30),
                                  "main timeout", "timeout")

    def test_main_spawn_failure_exits_2(self):
        self._assert_main_exits_2(OSError(2, "missing binary"),
                                  "main spawn failure", "spawn")

    def test_main_nonzero_exits_2_with_next_action(self):
        code, out, err = self._run_main(_fail_bytes())
        self.assertEqual(code, 2, "main must exit 2 (nonzero)")
        combined = out + err
        self.assertIn("floor-guard:", combined, "must keep the prefix (nonzero)")
        self.assert_next_action(combined, "nonzero")
        self.assert_no_traceback(combined, "nonzero")


class SquawkGitTests(TimeoutPolicyAssertions, unittest.TestCase):
    def test_diff_argv_preserved(self):
        payload = b"a.sql\x00"
        with mock.patch.object(squawk.subprocess, "run",
                               return_value=_ok_bytes(stdout=payload)) as run:
            self.assertEqual(squawk.git(Path("/tmp"), "diff", "--"), ["a.sql", ""])
        (argv,), kwargs = run.call_args
        self.assertEqual(argv[:3], ["git", "-C", "/tmp"],
                         "git argv prefix must be preserved")
        self.assertTrue(kwargs.get("capture_output"), "git must stay captured")

    def test_both_calls_are_bounded_with_one_policy_value(self):
        calls = []

        def recording(*argv, **kwargs):
            calls.append(kwargs)
            if argv and argv[0][3:4] == ["rev-parse"]:
                return _ok_bytes(stdout=b"abc123")
            return _ok_bytes(stdout=b"\x00")

        with mock.patch.object(squawk.subprocess, "run", side_effect=recording):
            squawk.changed_paths(Path("/tmp"), "main")
        self.assertEqual(len(calls), 2, "rev-parse plus diff must both run")
        values = [self.assert_valid_timeout(kwargs, f"squawk call {index}")
                  for index, kwargs in enumerate(calls)]
        self.assertEqual(values[0], values[1],
                         "one tool must use one policy value")

    def _mapped_message(self, effect):
        with mock.patch.object(squawk.subprocess, "run", **_mock_kwargs(effect)):
            try:
                squawk.git(Path("/tmp"), "diff", "--")
            except squawk.SquawkError as error:
                return str(error)
            except RAW_SUBPROCESS_ERRORS as error:
                self.fail(f"must map raw {type(error).__name__} to SquawkError")
        self.fail("a failed git read must refuse, never pass silently")

    def test_timeout_maps_to_squawk_error(self):
        message = self._mapped_message(subprocess.TimeoutExpired("git", 30))
        self.assertIn("Git baseline", message, "must keep the existing prefix")
        self.assertIn("timeout", message.lower(), "must name the class")
        self.assert_next_action(message, "squawk timeout")

    def test_spawn_failure_maps_to_squawk_error(self):
        message = self._mapped_message(OSError(2, "missing binary"))
        self.assertIn("Git baseline", message, "must keep the existing prefix")
        self.assertTrue("spawn" in message.lower() or "unavailable" in message.lower(),
                        "must name the class")
        self.assert_next_action(message, "squawk spawn failure")

    def test_nonzero_keeps_prefix_without_echo(self):
        message = self._mapped_message(_fail_bytes(stderr=SENTINEL.encode()))
        self.assertIn("Git baseline", message, "must keep the existing prefix")
        self.assert_next_action(message, "squawk nonzero")
        self.assert_no_echo(message, "squawk nonzero")

    def test_rev_parse_timeout_maps_to_squawk_error(self):
        with mock.patch.object(squawk.subprocess, "run",
                               side_effect=subprocess.TimeoutExpired("git", 30)):
            try:
                squawk.changed_paths(Path("/tmp"), "main")
            except squawk.SquawkError as error:
                message = str(error)
            except RAW_SUBPROCESS_ERRORS as error:
                self.fail(f"must map raw {type(error).__name__} to SquawkError")
            else:
                self.fail("a failed rev-parse must refuse")
        self.assertIn("baseline", message.lower(), "must name the baseline")
        self.assertIn("timeout", message.lower(), "must name the class")
        self.assert_next_action(message, "rev-parse timeout")

    def test_rev_parse_spawn_failure_maps_to_squawk_error(self):
        with mock.patch.object(squawk.subprocess, "run",
                               side_effect=OSError(2, "missing binary")):
            try:
                squawk.changed_paths(Path("/tmp"), "main")
            except squawk.SquawkError as error:
                message = str(error)
            except RAW_SUBPROCESS_ERRORS as error:
                self.fail(f"must map raw {type(error).__name__} to SquawkError")
            else:
                self.fail("a failed rev-parse must refuse")
        self.assertIn("baseline", message.lower(), "must name the baseline")
        self.assertTrue("spawn" in message.lower() or "unavailable" in message.lower(),
                        "must name the class")
        self.assert_next_action(message, "rev-parse spawn failure")

    def _run_main(self, effect):
        with tempfile.TemporaryDirectory() as scratch:
            out, err = io.StringIO(), io.StringIO()
            with mock.patch.object(squawk.subprocess, "run", **_mock_kwargs(effect)), \
                    contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                try:
                    code = squawk.main(["--root", scratch, "--base-ref", "main"])
                except RAW_SUBPROCESS_ERRORS as error:
                    self.fail(f"main must map raw {type(error).__name__}")
            return code, out.getvalue(), err.getvalue()

    def _assert_main_refuses(self, effect, label, needle):
        code, out, err = self._run_main(effect)
        self.assertEqual(code, 1, f"main must exit 1 ({label})")
        combined = out + err
        self.assertIn("squawk gate:", combined, f"must keep the prefix ({label})")
        self.assertIn(needle, combined.lower(), f"must name the class ({label})")
        self.assert_next_action(combined, label)
        self.assert_no_echo(combined, label)
        self.assert_no_traceback(combined, label)
        self.assertNotIn("no risky migration statements", combined,
                         f"must never print clean ({label})")

    def test_main_timeout_refuses_cleanly(self):
        self._assert_main_refuses(subprocess.TimeoutExpired("git", 30),
                                  "main timeout", "timeout")

    def test_main_spawn_failure_refuses_cleanly(self):
        self._assert_main_refuses(OSError(2, "missing binary"),
                                  "main spawn failure", "spawn")

    def test_main_nonzero_refuses_cleanly(self):
        self._assert_main_refuses(_fail_bytes(stderr=SENTINEL.encode()),
                                  "main nonzero", "baseline")


class ContainerSelectorTests(TimeoutPolicyAssertions, unittest.TestCase):
    SELECTION = re.compile(r"^(true|false)$", re.MULTILINE)

    def test_argv_and_modes_preserved(self):
        with mock.patch.object(container.subprocess, "run",
                               return_value=_ok_text(
                                   stdout="docs/a.md\n")) as run:
            self.assertEqual(container.changed_files("a", "b", root=Path("/tmp")),
                             ["docs/a.md"])
        (argv,), kwargs = run.call_args
        self.assertEqual(
            argv,
            ["git", "-C", "/tmp", "diff", "--name-only", "--no-renames", "a..b"],
            "diff argv must be preserved exactly",
        )
        self.assertTrue(kwargs.get("capture_output"), "must stay captured")
        self.assertTrue(kwargs.get("text"), "must stay in text mode")
        self.assertTrue(kwargs.get("check"), "must keep check=True")

    def test_call_is_bounded(self):
        with mock.patch.object(container.subprocess, "run",
                               return_value=_ok_text(stdout="x\n")) as run:
            container.changed_files("a", "b")
        _, kwargs = run.call_args
        self.assert_valid_timeout(kwargs, "container diff")

    def _assert_never_returns_a_diff(self, effect, label):
        with mock.patch.object(container.subprocess, "run", **_mock_kwargs(effect)):
            try:
                result = container.changed_files("a", "b")
            except RAW_SUBPROCESS_ERRORS as error:
                self.fail(f"must map raw {type(error).__name__} ({label})")
            except Exception:
                return
        self.fail(f"a failed diff must refuse, never return ({label})")

    def test_timeout_never_returns_a_diff(self):
        self._assert_never_returns_a_diff(subprocess.TimeoutExpired("git", 30),
                                          "timeout")

    def test_spawn_failure_never_returns_a_diff(self):
        self._assert_never_returns_a_diff(OSError(2, "missing binary"),
                                          "spawn failure")

    def _run_main(self, effect):
        out, err = io.StringIO(), io.StringIO()
        with mock.patch.object(container.subprocess, "run", **_mock_kwargs(effect)), \
                contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            try:
                code = container.main(["--base-ref", "a", "--head-ref", "b"])
            except RAW_SUBPROCESS_ERRORS as error:
                self.fail(f"main must map raw {type(error).__name__}")
        return code, out.getvalue(), err.getvalue()

    def _assert_main_selects_full_build(self, effect, label, needle):
        code, out, err = self._run_main(effect)
        self.assertEqual(code, 2, f"main must exit 2 ({label})")
        combined = out + err
        self.assertIn("container-inputs:", combined,
                      f"must keep the prefix ({label})")
        self.assertIn("selecting a full build", combined,
                      f"must keep the fallback wording ({label})")
        self.assertIn(needle, combined.lower(), f"must name the class ({label})")
        self.assert_next_action(combined, label)
        self.assert_no_traceback(combined, label)
        self.assertIsNone(self.SELECTION.search(combined),
                          f"must emit no selection line ({label})")

    def test_main_timeout_selects_full_build(self):
        self._assert_main_selects_full_build(subprocess.TimeoutExpired("git", 30),
                                             "main timeout", "timeout")

    def test_main_spawn_failure_selects_full_build(self):
        self._assert_main_selects_full_build(OSError(2, "missing binary"),
                                             "main spawn failure", "spawn")

    def test_main_nonzero_selects_full_build_with_next_action(self):
        self._assert_main_selects_full_build(
            subprocess.CalledProcessError(128, "git"),
            "main nonzero", "failed")

    def test_valid_docs_diff_still_skips(self):
        code, out, _ = self._run_main(_ok_text(stdout="docs/runbook.md\n"))
        self.assertEqual(code, 0)
        self.assertEqual(out, "false\n")

    def test_valid_image_diff_still_builds(self):
        code, out, _ = self._run_main(_ok_text(stdout="crates/core/src/lib.rs\n"))
        self.assertEqual(code, 0)
        self.assertEqual(out, "true\n")


class JobSelectorTests(TimeoutPolicyAssertions, unittest.TestCase):
    SELECTION = re.compile(r"^(rust|worker|parity|supply|docs|voice)=(true|false)$",
                           re.MULTILINE)

    def test_argv_variants_preserved(self):
        seen = []

        def recording(command, **kwargs):
            seen.append((command, kwargs))
            return _ok_text(stdout="docs/a.md\n")

        with mock.patch.object(job.subprocess, "run", side_effect=recording):
            job.git_diff_names("a", "b", root=Path("/tmp"), diff_filter="ACMRTUXB")
            job.git_diff_names("a", "b", root=Path("/tmp"), diff_filter="D")
            job.git_diff_names("a", "b", root=Path("/tmp"))
        self.assertEqual(
            [command for command, _ in seen],
            [["git", "-C", "/tmp", "diff", "--name-only", "--no-renames",
              "--diff-filter=ACMRTUXB", "a..b"],
             ["git", "-C", "/tmp", "diff", "--name-only", "--no-renames",
              "--diff-filter=D", "a..b"],
             ["git", "-C", "/tmp", "diff", "--name-only", "--no-renames", "a..b"]],
            "diff argv variants must be preserved exactly",
        )
        for _, kwargs in seen:
            self.assertTrue(kwargs.get("capture_output"), "must stay captured")
            self.assertTrue(kwargs.get("text"), "must stay in text mode")
            self.assertTrue(kwargs.get("check"), "must keep check=True")

    def test_both_main_calls_are_bounded_with_one_policy_value(self):
        calls = []

        def recording(command, **kwargs):
            calls.append(kwargs)
            return _ok_text(stdout="docs/a.md\n")

        out = io.StringIO()
        with mock.patch.object(job.subprocess, "run", side_effect=recording), \
                contextlib.redirect_stdout(out):
            code = job.main(["--base-ref", "a", "--head-ref", "b"])
        self.assertEqual(code, 0)
        self.assertEqual(len(calls), 2, "changed plus deleted diffs must both run")
        values = [self.assert_valid_timeout(kwargs, f"job call {index}")
                  for index, kwargs in enumerate(calls)]
        self.assertEqual(values[0], values[1],
                         "one tool must use one policy value")

    def _assert_never_returns_a_diff(self, effect, label):
        with mock.patch.object(job.subprocess, "run", **_mock_kwargs(effect)):
            try:
                job.git_diff_names("a", "b")
            except RAW_SUBPROCESS_ERRORS as error:
                self.fail(f"must map raw {type(error).__name__} ({label})")
            except Exception:
                return
        self.fail(f"a failed diff must refuse, never return ({label})")

    def test_timeout_never_returns_a_diff(self):
        self._assert_never_returns_a_diff(subprocess.TimeoutExpired("git", 30),
                                          "timeout")

    def test_spawn_failure_never_returns_a_diff(self):
        self._assert_never_returns_a_diff(OSError(2, "missing binary"),
                                          "spawn failure")

    def _run_main(self, effect):
        out, err = io.StringIO(), io.StringIO()
        with mock.patch.object(job.subprocess, "run", **_mock_kwargs(effect)), \
                contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            try:
                code = job.main(["--base-ref", "a", "--head-ref", "b"])
            except RAW_SUBPROCESS_ERRORS as error:
                self.fail(f"main must map raw {type(error).__name__}")
        return code, out.getvalue(), err.getvalue()

    def _assert_main_selects_full_jobs(self, effect, label, needle):
        code, out, err = self._run_main(effect)
        self.assertEqual(code, 2, f"main must exit 2 ({label})")
        combined = out + err
        self.assertIn("job-inputs:", combined, f"must keep the prefix ({label})")
        self.assertIn("selecting full jobs", combined,
                      f"must keep the fallback wording ({label})")
        self.assertIn(needle, combined.lower(), f"must name the class ({label})")
        self.assert_next_action(combined, label)
        self.assert_no_traceback(combined, label)
        self.assertIsNone(self.SELECTION.search(combined),
                          f"must emit no selection line ({label})")

    def test_main_timeout_selects_full_jobs(self):
        self._assert_main_selects_full_jobs(subprocess.TimeoutExpired("git", 30),
                                            "main timeout", "timeout")

    def test_main_spawn_failure_selects_full_jobs(self):
        self._assert_main_selects_full_jobs(OSError(2, "missing binary"),
                                            "main spawn failure", "spawn")

    def test_main_nonzero_selects_full_jobs_with_next_action(self):
        self._assert_main_selects_full_jobs(subprocess.CalledProcessError(128, "git"),
                                            "main nonzero", "failed")

    def test_second_diff_failure_selects_full_jobs(self):
        out, err = io.StringIO(), io.StringIO()
        effects = [_ok_text(stdout="docs/a.md\n"),
                   subprocess.TimeoutExpired("git", 30)]
        with mock.patch.object(job.subprocess, "run", side_effect=effects), \
                contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            try:
                code = job.main(["--base-ref", "a", "--head-ref", "b"])
            except RAW_SUBPROCESS_ERRORS as error:
                self.fail(f"main must map raw {type(error).__name__} (second diff)")
        self.assertEqual(code, 2, "a failed deleted-diff must exit 2")
        combined = out.getvalue() + err.getvalue()
        self.assertIn("job-inputs:", combined, "must keep the prefix (second diff)")
        self.assert_next_action(combined, "second diff")
        self.assertIsNone(self.SELECTION.search(combined),
                          "must emit no selection line (second diff)")

    def test_valid_docs_diff_output_unchanged(self):
        code, out, _ = self._run_main([_ok_text(stdout="docs/runbook.md\n"),
                                       _ok_text(stdout="")])
        self.assertEqual(code, 0)
        # Proven classification: an ordinary docs page selects the worker job
        # only (runbook.test.ts asserts on the docs/ listing itself).
        self.assertIn("rust=false", out)
        self.assertIn("worker=true", out)
        self.assertIn("supply=false", out)
        self.assertIn("docs=true", out)

    def test_valid_rust_diff_output_unchanged(self):
        code, out, _ = self._run_main(
            [_ok_text(stdout="crates/core/src/lib.rs\n"), _ok_text(stdout="")])
        self.assertEqual(code, 0)
        self.assertIn("rust=true", out)
        self.assertIn("worker=true", out)
        self.assertIn("supply=true", out)


if __name__ == "__main__":
    unittest.main()
