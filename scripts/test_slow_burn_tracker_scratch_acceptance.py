"""Protected offline acceptance: scratch I/O failures are unknown, not actions.

Inject failures at stdlib filesystem boundaries, not tracker helpers. No real
scrape, node or GitHub command can run. Existing evidence has its own writable
directory, so a ring scratch failure must still produce a fresh safe receipt.
"""

import builtins
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
tracker = importlib.import_module("slow_burn_tracker")

NOW = 1_700_000_000
ERROR_MARKER = "private-os-error-marker"
ENV = {"GITHUB_REPOSITORY": "TogetherWeOwn/two-bot-next",
       "GITHUB_SERVER_URL": "https://github.com", "GITHUB_RUN_ID": "1",
       "GH_TOKEN": "synthetic-github-marker",
       "METRICS_SCRAPE_TOKEN": "synthetic-scrape-marker"}
FIRE = {"slow": True, "fast": False, "held": False,
        "slowCovered": True,
        "ring": [{"t": NOW, "received": 50, "duplicate": 0, "fastMoves": 48}]}


class TrackerScratchAcceptanceTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory(prefix="tracker-scratch-acceptance-")
        self.addCleanup(tmp.cleanup)
        self.dir = Path(tmp.name)
        previous = Path.cwd()
        os.chdir(self.dir)
        self.addCleanup(os.chdir, previous)
        self.expo_dir = self.dir / "exposition"
        self.expo_dir.mkdir()
        self.expo = self.expo_dir / "expo.prom"
        self.expo.write_text("previous exposition\n")
        self.receipts = self.dir / "receipts"
        self.receipts.mkdir()
        self.evidence = self.receipts / "receipt.json"
        state = self.dir / "state"
        state.mkdir()
        self.ring = state / "ring.json"
        self.ring.write_text('[{"t":1699999700,"received":25,"duplicate":0,"fastMoves":24}]')
        self.coverage = state / "coverage.json"
        self.coverage.write_text('{"last_covered":1699996400}')
        self.sidecar = self.dir / "slow-burn-ring-prev.json"
        self.sidecar.write_bytes(b"tracked sidecar sentinel\n")
        self.before = {path: path.read_bytes()
                       for path in (self.ring, self.coverage, self.sidecar)}
        self.gh_calls = []
        self.driver_paths = []
        self.argv = ["--fixture", "fire", "--evidence-out", str(self.evidence),
                     "--exposition-out", str(self.expo), "--ring", str(self.ring),
                     "--coverage-path", str(self.coverage)]

        def forbidden_gh(*args, **kwargs):
            self.gh_calls.append(args)
            raise AssertionError("scratch refusal must not reach GitHub")

        for target, name, effect in (
                (tracker, "gh", forbidden_gh),
                (tracker.subprocess, "run", AssertionError("offline test must not execute node/gh")),
                (tracker.urllib.request, "build_opener", AssertionError("offline test must not scrape"))):
            patcher = mock.patch.object(target, name, side_effect=effect)
            patcher.start()
            self.addCleanup(patcher.stop)

    def driver(self, node, driver, exposition_path, ring_path, now):
        ring = Path(ring_path)
        self.driver_paths.append(ring)
        self.assertTrue(ring.is_file())
        self.assertIsInstance(json.loads(ring.read_text()), list)
        self.assertEqual(ring.parent.parent.resolve(), self.expo_dir.resolve())
        self.assertTrue(ring.parent.name.startswith("slow-burn-ring-"))
        self.assertTrue(Path(exposition_path).is_file())
        return dict(FIRE)

    @contextlib.contextmanager
    def fail_scratch(self, phase, error):
        """One filesystem fault; unrelated receipt/state paths stay writable."""
        injected = []
        directories = []
        real_mkdtemp = tempfile.mkdtemp
        real_io_open = io.open
        real_builtin_open = builtins.open
        real_cleanup = tempfile.TemporaryDirectory.cleanup

        def inject():
            injected.append(phase)
            raise error

        def mkdtemp(suffix=None, prefix=None, dir=None):
            if phase == "setup" and dir is not None and Path(dir).resolve() == self.expo_dir.resolve():
                inject()
            return real_mkdtemp(suffix=suffix, prefix=prefix, dir=dir)

        def scratch_path(path):
            return (not isinstance(path, int)
                    and Path(path).parent.parent.resolve() == self.expo_dir.resolve()
                    and Path(path).parent.name.startswith("slow-burn-ring-"))

        def guarded_open(real_open, file, mode="r", *args, **kwargs):
            if phase == "write" and any(flag in mode for flag in "wax+") and scratch_path(file):
                inject()
            return real_open(file, mode, *args, **kwargs)

        def cleanup(directory):
            # Inject at the public cleanup boundary: a low-level rmdir fault
            # may be retried inside the stdlib and is not itself a failed
            # cleanup operation visible to the tracker.
            directories.append(directory)
            if phase == "cleanup" and Path(directory.name).parent.resolve() == self.expo_dir.resolve():
                inject()
            return real_cleanup(directory)

        try:
            with contextlib.ExitStack() as stack:
                stack.enter_context(mock.patch.object(tempfile, "mkdtemp", side_effect=mkdtemp))
                stack.enter_context(mock.patch.object(
                    io, "open", side_effect=lambda *a, **kw: guarded_open(real_io_open, *a, **kw)))
                stack.enter_context(mock.patch.object(
                    builtins, "open", side_effect=lambda *a, **kw: guarded_open(real_builtin_open, *a, **kw)))
                stack.enter_context(mock.patch.object(tempfile.TemporaryDirectory, "cleanup", cleanup))
                yield injected
        finally:
            # Own failed fixture cleanup explicitly instead of letting a later
            # GC finalizer inject an unrelated ResourceWarning into output.
            for directory in directories:
                real_cleanup(directory)

    def check_failure(self, phase, error_class):
        diagnostics = []
        for detail in ("first", "different"):
            with self.subTest(detail=detail):
                self.evidence.unlink(missing_ok=True)
                self.gh_calls.clear()
                self.driver_paths.clear()
                errno = 13 if error_class is PermissionError else 5
                error = error_class(errno, ERROR_MARKER + "-" + detail, str(self.expo_dir))
                output = io.StringIO()
                escaped = None
                code = None
                with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
                    with mock.patch.object(tracker.time, "time", return_value=NOW):
                        with mock.patch.object(tracker, "run_driver", side_effect=self.driver):
                            with self.fail_scratch(phase, error) as injected:
                                try:
                                    code = tracker.main(list(self.argv), dict(ENV))
                                except Exception as caught:
                                    escaped = type(caught).__name__
                problems = []
                if not injected:
                    problems.append("fixture did not exercise the scratch fault")
                if escaped:
                    problems.append(f"escaped {escaped} instead of returning 1")
                if code != 1:
                    problems.append(f"return code {code!r}, expected 1")
                if self.gh_calls:
                    problems.append("scratch failure reached GitHub")
                if len(self.driver_paths) != (1 if phase == "cleanup" else 0):
                    problems.append("driver ran at the wrong failure boundary")
                if not self.evidence.is_file():
                    problems.append("no fresh separately writable evidence receipt")
                    evidence_text = ""
                else:
                    evidence_text = self.evidence.read_text()
                    receipt = json.loads(evidence_text)
                    if (receipt.get("verdict"), receipt.get("action"), receipt.get("issue")) != ("unknown", "none", None):
                        problems.append("receipt is not unknown/no-action/no-issue")
                diagnostic = output.getvalue().strip()
                if not diagnostic.startswith("::error::burn evaluation failed:") or len(diagnostic) > 160:
                    problems.append("missing bounded fixed burn-evaluation diagnostic")
                for unsafe in (ERROR_MARKER, str(self.dir), "PermissionError", "OSError", "Traceback",
                               ENV["GH_TOKEN"], ENV["METRICS_SCRAPE_TOKEN"]):
                    if unsafe in diagnostic or unsafe in evidence_text:
                        problems.append("raw OS detail, path or token escaped")
                if any(path.read_bytes() != before for path, before in self.before.items()):
                    problems.append("failed evaluation changed persistent ring/coverage/sidecar")
                diagnostics.append(diagnostic)
                self.assertEqual(problems, [], "; ".join(problems))
        self.assertEqual(len(set(diagnostics)), 1, "OS error text must not choose the diagnostic")

    def test_scratch_setup_permission_error_is_safe_unknown(self):
        self.check_failure("setup", PermissionError)

    def test_scratch_write_permission_error_is_safe_unknown(self):
        self.check_failure("write", PermissionError)

    def test_scratch_cleanup_permission_error_is_safe_unknown(self):
        self.check_failure("cleanup", PermissionError)

    def test_scratch_setup_os_error_is_safe_unknown(self):
        self.check_failure("setup", OSError)

    def test_scratch_write_os_error_is_safe_unknown(self):
        self.check_failure("write", OSError)

    def test_scratch_cleanup_os_error_is_safe_unknown(self):
        self.check_failure("cleanup", OSError)

    def test_writable_scratch_keeps_unique_inputs_and_shared_sidecar(self):
        def held(*args):
            self.driver(*args)
            return dict(FIRE, slow=False, held=True, slowCovered=False)

        with mock.patch.object(tracker, "run_driver", side_effect=held):
            with contextlib.redirect_stdout(io.StringIO()):
                for _ in range(2):
                    self.assertEqual(tracker.main(list(self.argv), dict(ENV)), 0)
        self.assertEqual(len(self.driver_paths), 2)
        self.assertNotEqual(self.driver_paths[0], self.driver_paths[1])
        for path in self.driver_paths:
            self.assertFalse(path.exists())
            self.assertFalse(path.parent.exists())
        self.assertEqual(list(self.expo_dir.glob("slow-burn-ring-*")), [])
        self.assertEqual(self.gh_calls, [])
        receipt = json.loads(self.evidence.read_text())
        self.assertEqual((receipt["verdict"], receipt["action"], receipt["issue"]),
                         ("held", "none", None))
        for path, before in self.before.items():
            self.assertEqual(path.read_bytes(), before)


if __name__ == "__main__":
    unittest.main()
