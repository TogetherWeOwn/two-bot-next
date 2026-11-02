#!/usr/bin/env python3
"""Acceptance tests: bound the retention-audit provenance Git subprocesses.

Cache-provenance half of the bounded-subprocess bundle: the three Git
calls in ``scripts/cargo_cache.py`` ``retention_audit`` (``rev-parse``
``--show-toplevel``, ``ls-files -z -- target``, ``check-ignore -q
target``) must gain finite timeouts no longer than the remaining receipt
freshness allowance, freshness must be rechecked after the waits, and
every timeout, spawn/OS failure and nonzero Git result must leave the
affected candidate ineligible -- or refuse the whole audit -- with a
fixed machine-identifiable reason plus a next action. Uncertainty must
never count as an empty index, an ignored target, or proved repository
ownership.

Locked contract
---------------
- Each of the three calls passes ``timeout=`` with a finite value in the
  envelope ``1..120`` seconds and never longer than the remaining
  freshness allowance (``max_age - (now - captured_at_unix)``). The
  Builder picks the exact value(s) and documents them as the local
  policy; ``argv``, capture mode, ``check`` mode and ``GIT_*``
  neutralization are preserved exactly.
- A timeout at any of the three sites refuses with a reason containing
  ``git-timeout`` and a ``next:`` action. A spawn/OS failure refuses
  with ``git-spawn`` and ``next:``. ``rev-parse`` nonzero refuses with
  ``git-unverified`` and ``next:``; ``ls-files`` nonzero with
  ``git-ls-files`` and ``next:``; ``check-ignore`` error exits (>= 2)
  with ``git-ignore`` and ``next:``. Each refusal is either the
  candidate's ineligibility reason or a whole-audit ``Refusal``; both
  shapes are fail-closed and both are accepted.
- Legitimate outcomes are preserved exactly: ``check-ignore`` exit 1
  stays the plain not-ignored ineligibility, an exact-toplevel mismatch
  stays the unverified-repository ineligibility, missing Operator
  classification stays ineligible without running Git, and the eligible
  control keeps the audit-only output (``audit_only``, ``version`` 1,
  ``audit only; not deletion authority``).
- Freshness is enforced after the waits: an audit whose Git waits push
  it past ``max_age`` raises ``Refusal`` even when every Git output
  would otherwise qualify the candidate.
- The supervised Cargo ``Popen`` is intentionally excluded and must not
  gain a timeout.
- RED is behavioral: every failing assertion fails on a timeout kwarg,
  eligibility flag, reason token or refusal comparison against live
  code paths with mocked subprocesses, a monotonic clock double and
  synthetic temporary trees -- never on an import error, a missing
  fixture, a shared pool scan, retention/delete, real cache access or a
  compile.

Protected file: the Builder builds on this commit and must never edit
this file; test corrections go back to its author.

Run: PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts/ci -p 'test_bounded_subprocess_cache_acceptance.py' -v
"""

import importlib.util
import math
import os
import subprocess
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

HERE = Path(__file__).resolve().parent
SCRIPTS = HERE.parent

SPEC = importlib.util.spec_from_file_location(
    "cargo_cache_bounded", SCRIPTS / "cargo_cache.py")
cache = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(cache)

TIMEOUT_MIN = 1
TIMEOUT_MAX = 120
NEUTRALIZED = {"GIT_INDEX_FILE", "GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR",
               "GIT_NAMESPACE"}

RAW_SUBPROCESS_ERRORS = (
    subprocess.TimeoutExpired,
    subprocess.CalledProcessError,
    OSError,
)


def _ok_rev_parse(workspace):
    return subprocess.CompletedProcess(
        ("git",), 0, stdout=f"{workspace}\n", stderr="")


def _ok_ls_files():
    return subprocess.CompletedProcess(("git",), 0, stdout=b"", stderr=b"")


def _ok_check_ignore(status=0):
    return subprocess.CompletedProcess(("git",), status, stdout=b"", stderr=b"")


class AuditFixture(unittest.TestCase):
    """Synthetic worktree + inventory that reaches the provenance Git calls."""

    def setUp(self):
        scratch = os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")
        self.temp = tempfile.TemporaryDirectory(dir=scratch)
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

    def run_audit(self, effect, age=0):
        """Run the audit with one mocked Git behavior; return calls + outcome."""
        inventory = dict(self.inventory)
        inventory["captured_at_unix"] = self.now - age
        calls = []

        def recording(argv, **kwargs):
            calls.append((argv, kwargs))
            if isinstance(effect, Exception):
                raise effect
            result = effect(argv, **kwargs)
            # Faithful check=True semantics: the real subprocess.run raises
            # on nonzero, so the mock must too -- otherwise a nonzero result
            # would slip past a check=True call site it could never pass live.
            if kwargs.get("check") and result.returncode != 0:
                raise subprocess.CalledProcessError(
                    result.returncode, argv, output=result.stdout,
                    stderr=result.stderr)
            return result

        with mock.patch.object(cache.subprocess, "run", side_effect=recording):
            try:
                result = cache.retention_audit(
                    self.worktrees, inventory, proc_root=str(self.root),
                    now=self.now, max_age=self.max_age)
            except cache.Refusal as refusal:
                return calls, ("refused", str(refusal))
            except RAW_SUBPROCESS_ERRORS as error:
                self.fail(f"must map raw {type(error).__name__}, never propagate it")
        return calls, ("audited", result)

    def assert_valid_timeout(self, kwargs, label, allowance):
        # Manual membership check: kwargs carries the full child env, and an
        # assertIn failure would print it (secrets) into the test log.
        if "timeout" not in kwargs:
            self.fail(f"missing timeout kwarg ({label})")
        value = kwargs["timeout"]
        self.assertIsInstance(value, (int, float),
                              f"timeout must be numeric ({label})")
        self.assertNotIsInstance(value, bool, f"timeout must be numeric ({label})")
        self.assertTrue(math.isfinite(value), f"timeout must be finite ({label})")
        self.assertGreaterEqual(value, TIMEOUT_MIN,
                                f"timeout below policy floor ({label})")
        self.assertLessEqual(value, TIMEOUT_MAX,
                             f"timeout above policy ceiling ({label})")
        self.assertLessEqual(value, allowance,
                             f"timeout exceeds the freshness allowance ({label})")
        return value

    def assert_refusal_tokens(self, outcome, token, label):
        kind, payload = outcome
        if kind == "refused":
            message = payload
            self.assertIn(token, message, f"Refusal must carry {token} ({label})")
            self.assertIn("next:", message.lower(),
                          f"Refusal must carry a next action ({label})")
            return None
        self.assertEqual(kind, "audited", f"audit must refuse, not crash ({label})")
        candidates = payload["candidates"]
        self.assertEqual(len(candidates), 1, "fixture holds one candidate")
        row = candidates[0]
        self.assertFalse(row["eligible"],
                         f"uncertainty must never qualify ({label})")
        self.assertIn(token, row["reason"],
                      f"reason must carry {token} ({label})")
        self.assertIn("next:", row["reason"].lower(),
                      f"reason must carry a next action ({label})")
        return row

    def success_effect(self, workspace=None, ignore_status=0):
        target = workspace if workspace is not None else self.workspace

        def effect(argv, **kwargs):
            if argv[3:4] == ["rev-parse"]:
                return _ok_rev_parse(target)
            if argv[3:4] == ["ls-files"]:
                return _ok_ls_files()
            if argv[3:4] == ["check-ignore"]:
                return _ok_check_ignore(ignore_status)
            raise AssertionError("unexpected provenance argv")

        return effect


class TimeoutBoundTests(AuditFixture):
    def test_each_call_is_bounded_within_the_allowance(self):
        calls, outcome = self.run_audit(self.success_effect())
        kind, _ = outcome
        self.assertEqual(kind, "audited", "the success path must audit")
        self.assertEqual(len(calls), 3, "rev-parse, ls-files and check-ignore must run")
        verbs = [argv[3] for argv, _ in calls]
        self.assertEqual(verbs, ["rev-parse", "ls-files", "check-ignore"],
                         "provenance argv must be preserved in order")
        for (argv, kwargs), verb in zip(calls, verbs):
            self.assert_valid_timeout(kwargs, verb, self.max_age)

    def test_bounded_by_the_remaining_allowance_on_an_aged_receipt(self):
        calls, outcome = self.run_audit(self.success_effect(), age=50)
        kind, _ = outcome
        self.assertEqual(kind, "audited", "an aged receipt must still audit")
        self.assertEqual(len(calls), 3, "all three calls must run")
        for (argv, kwargs), verb in zip(
                calls, ["rev-parse", "ls-files", "check-ignore"]):
            self.assert_valid_timeout(kwargs, f"aged {verb}", 10)

    def test_argv_capture_and_check_modes_preserved(self):
        calls, _ = self.run_audit(self.success_effect())
        self.assertEqual(len(calls), 3, "all three calls must run")
        (rev_argv, rev_kwargs), (ls_argv, ls_kwargs), (ig_argv, ig_kwargs) = calls
        workspace = str(self.workspace)
        self.assertEqual(rev_argv,
                         ["git", "-C", workspace, "rev-parse", "--show-toplevel"],
                         "rev-parse argv must be preserved")
        self.assertTrue(rev_kwargs.get("check"), "rev-parse must keep check=True")
        self.assertTrue(rev_kwargs.get("text"), "rev-parse must stay in text mode")
        self.assertEqual(ls_argv,
                         ["git", "-C", workspace, "ls-files", "-z", "--", "target"],
                         "ls-files argv must be preserved")
        self.assertTrue(ls_kwargs.get("check"), "ls-files must keep check=True")
        self.assertEqual(ig_argv,
                         ["git", "-C", workspace, "check-ignore", "-q", "target"],
                         "check-ignore argv must be preserved")
        # Manual check: ig_kwargs carries the full child env, which must
        # never appear in a failure message.
        if "check" in ig_kwargs:
            self.fail("check-ignore must stay unchecked (exit codes are data)")


class RevParseFailureTests(AuditFixture):
    def test_timeout_refuses_with_token(self):
        _, outcome = self.run_audit(subprocess.TimeoutExpired("git", 30))
        self.assert_refusal_tokens(outcome, "git-timeout", "rev-parse timeout")

    def test_spawn_failure_refuses_with_token(self):
        _, outcome = self.run_audit(OSError(2, "missing binary"))
        self.assert_refusal_tokens(outcome, "git-spawn", "rev-parse spawn failure")

    def test_nonzero_refuses_with_token(self):
        def effect(argv, **kwargs):
            if argv[3:4] == ["rev-parse"]:
                raise subprocess.CalledProcessError(128, argv)
            return self.success_effect()(argv, **kwargs)

        _, outcome = self.run_audit(effect)
        self.assert_refusal_tokens(outcome, "git-unverified", "rev-parse nonzero")


class LsFilesFailureTests(AuditFixture):
    def _after_rev_parse(self, failure):
        def effect(argv, **kwargs):
            if argv[3:4] == ["rev-parse"]:
                return _ok_rev_parse(self.workspace)
            if argv[3:4] == ["ls-files"]:
                if isinstance(failure, Exception):
                    raise failure
                return failure
            return _ok_check_ignore()

        return effect

    def test_timeout_refuses_with_token(self):
        _, outcome = self.run_audit(
            self._after_rev_parse(subprocess.TimeoutExpired("git", 30)))
        self.assert_refusal_tokens(outcome, "git-timeout", "ls-files timeout")

    def test_spawn_failure_refuses_with_token(self):
        _, outcome = self.run_audit(
            self._after_rev_parse(OSError(2, "missing binary")))
        self.assert_refusal_tokens(outcome, "git-spawn", "ls-files spawn failure")

    def test_nonzero_never_counts_as_an_empty_index(self):
        failure = subprocess.CompletedProcess(("git",), 128, b"", b"")
        _, outcome = self.run_audit(self._after_rev_parse(failure))
        self.assert_refusal_tokens(outcome, "git-ls-files", "ls-files nonzero")


class CheckIgnoreFailureTests(AuditFixture):
    def _after_first_two(self, failure):
        def effect(argv, **kwargs):
            if argv[3:4] == ["rev-parse"]:
                return _ok_rev_parse(self.workspace)
            if argv[3:4] == ["ls-files"]:
                return _ok_ls_files()
            if isinstance(failure, Exception):
                raise failure
            return failure

        return effect

    def test_timeout_refuses_with_token(self):
        _, outcome = self.run_audit(
            self._after_first_two(subprocess.TimeoutExpired("git", 30)))
        self.assert_refusal_tokens(outcome, "git-timeout", "check-ignore timeout")

    def test_spawn_failure_refuses_with_token(self):
        _, outcome = self.run_audit(
            self._after_first_two(OSError(2, "missing binary")))
        self.assert_refusal_tokens(outcome, "git-spawn",
                                   "check-ignore spawn failure")

    def test_error_exit_refuses_with_token(self):
        failure = subprocess.CompletedProcess(("git",), 128, b"", b"")
        _, outcome = self.run_audit(self._after_first_two(failure))
        self.assert_refusal_tokens(outcome, "git-ignore", "check-ignore error")

    def test_exit_1_stays_the_plain_not_ignored_ineligibility(self):
        calls, outcome = self.run_audit(self.success_effect(ignore_status=1))
        kind, payload = outcome
        self.assertEqual(kind, "audited", "exit 1 must audit, not refuse")
        row = payload["candidates"][0]
        self.assertFalse(row["eligible"], "a not-ignored target stays ineligible")
        self.assertIn("not ignored", row["reason"],
                      "exit 1 must keep the existing wording")
        self.assertEqual(len(calls), 3, "all three calls must run")


class FreshnessAfterWaitsTests(AuditFixture):
    def test_slow_git_past_max_age_refuses_the_audit(self):
        started = [None]
        clock = {"now": 1000.0}

        def effect(argv, **kwargs):
            clock["now"] += 61
            return self.success_effect()(argv, **kwargs)

        def monotonic():
            if started[0] is None:
                started[0] = clock["now"]
            return clock["now"]

        with mock.patch.object(cache.subprocess, "run", side_effect=effect), \
                mock.patch.object(time, "monotonic", side_effect=monotonic):
            with self.assertRaises(cache.Refusal) as refusal:
                cache.retention_audit(
                    self.worktrees, dict(self.inventory),
                    proc_root=str(self.root), now=self.now, max_age=self.max_age)
        self.assertIn("new inventory", str(refusal.exception).lower(),
                      "the stale refusal must name the next action")

    def test_independent_candidates_refuse_independently(self):
        second = self.worktrees / "ws2"
        (second / "target" / "debug").mkdir(parents=True)
        inventory = dict(self.inventory)
        inventory["workspaces"] = [
            dict(self.inventory["workspaces"][0]),
            {"path": str(second), "live_run": False, "referenced": False,
             "issue_id": "TEST-2", "status": "done",
             "target_provenance": "build_output_only"},
        ]

        def effect(argv, **kwargs):
            workspace = argv[2]
            if workspace == str(self.workspace) and argv[3:4] == ["ls-files"]:
                raise subprocess.TimeoutExpired("git", 30)
            if argv[3:4] == ["rev-parse"]:
                return _ok_rev_parse(workspace)
            if argv[3:4] == ["ls-files"]:
                return _ok_ls_files()
            return _ok_check_ignore()

        with mock.patch.object(cache.subprocess, "run", side_effect=effect):
            try:
                result = cache.retention_audit(
                    self.worktrees, inventory, proc_root=str(self.root),
                    now=self.now, max_age=self.max_age)
            except cache.Refusal as refusal:
                self.assertIn("git-timeout", str(refusal),
                              "a whole-audit refusal must carry the token")
                self.assertIn("next:", str(refusal).lower(),
                              "a whole-audit refusal must name the next action")
                return
            except RAW_SUBPROCESS_ERRORS as error:
                self.fail(f"must map raw {type(error).__name__}, never propagate it")
        rows = {row["target"]: row for row in result["candidates"]}
        failed = rows[str(self.workspace / "target")]
        passed = rows[str(second / "target")]
        self.assertFalse(failed["eligible"], "the timed-out candidate must refuse")
        self.assertIn("git-timeout", failed["reason"],
                      "the failed reason must carry the token")
        self.assertTrue(passed["eligible"],
                        "the healthy candidate must stay eligible")
        self.assertEqual(passed["reason"], "audit only; not deletion authority",
                         "the healthy reason must be unchanged")


class PreservedControlsTests(AuditFixture):
    def test_eligible_control_keeps_the_audit_only_output(self):
        calls, outcome = self.run_audit(self.success_effect())
        kind, payload = outcome
        self.assertEqual(kind, "audited", "the success path must audit")
        self.assertEqual(payload["version"], 1, "output version must be 1")
        self.assertTrue(payload["audit_only"], "output must stay audit-only")
        self.assertEqual(payload["captured_at_unix"], self.now,
                         "capture stamp must be preserved")
        row = payload["candidates"][0]
        self.assertTrue(row["eligible"], "the clean candidate must stay eligible")
        self.assertEqual(row["reason"], "audit only; not deletion authority",
                         "the eligible reason must be unchanged")
        self.assertEqual(len(calls), 3, "all three calls must run")

    def test_git_environment_stays_neutralized(self):
        calls, _ = self.run_audit(self.success_effect())
        self.assertEqual(len(calls), 3, "all three calls must run")
        for _, kwargs in calls:
            env = kwargs.get("env", {})
            leaked = NEUTRALIZED & set(env)
            self.assertEqual(leaked, set(), "GIT_* overrides must stay neutralized")

    def test_neutralization_holds_under_hostile_overrides(self):
        hostile = {key: "/tmp/evil" for key in NEUTRALIZED}
        with mock.patch.dict(os.environ, hostile):
            calls, outcome = self.run_audit(self.success_effect())
        kind, _ = outcome
        self.assertEqual(kind, "audited", "hostile GIT_* must not break the audit")
        for _, kwargs in calls:
            env = kwargs.get("env", {})
            leaked = NEUTRALIZED & set(env)
            self.assertEqual(leaked, set(), "hostile GIT_* must stay neutralized")

    def test_toplevel_mismatch_stays_unverified(self):
        calls, outcome = self.run_audit(
            self.success_effect(workspace=self.root / "elsewhere"))
        kind, payload = outcome
        self.assertEqual(kind, "audited", "a mismatch must audit, not crash")
        row = payload["candidates"][0]
        self.assertFalse(row["eligible"], "a mismatched toplevel stays ineligible")
        self.assertIn("unverified", row["reason"].lower(),
                      "a mismatch must keep the existing wording")

    def test_missing_operator_classification_runs_no_git(self):
        inventory = dict(self.inventory)
        row = dict(inventory["workspaces"][0])
        del row["target_provenance"]
        inventory["workspaces"] = [row]
        with mock.patch.object(cache.subprocess, "run",
                               side_effect=AssertionError("git must not run")):
            result = cache.retention_audit(
                self.worktrees, inventory, proc_root=str(self.root),
                now=self.now, max_age=self.max_age)
        candidate = result["candidates"][0]
        self.assertFalse(candidate["eligible"],
                         "unclassified provenance stays ineligible")
        self.assertIn("classification", candidate["reason"].lower(),
                      "must keep the classification wording")

    def test_live_run_veto_needs_no_git(self):
        inventory = dict(self.inventory)
        row = dict(inventory["workspaces"][0])
        row["live_run"] = True
        inventory["workspaces"] = [row]
        with mock.patch.object(cache.subprocess, "run",
                               side_effect=AssertionError("git must not run")):
            result = cache.retention_audit(
                self.worktrees, inventory, proc_root=str(self.root),
                now=self.now, max_age=self.max_age)
        self.assertFalse(result["candidates"][0]["eligible"],
                         "a live run stays ineligible")

    def test_supervised_cargo_popen_stays_excluded(self):
        source = Path(cache.__file__).read_text()
        index = source.find("Popen(")
        self.assertNotEqual(index, -1, "the supervised Popen must still exist")
        window = source[index:index + 400]
        self.assertNotIn("timeout", window,
                         "the supervised Popen must not gain a timeout")


if __name__ == "__main__":
    unittest.main()
