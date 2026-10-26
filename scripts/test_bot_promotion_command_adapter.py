"""Offline spec for the bot_promotion._command adapter (no Docker/Wrangler).

Contract pinned through the real ``bot_promotion._command`` with real
subprocesses and temporary local Python fake executables only:

* success with a small local child works;
* output cap is enforced DURING reading (an unbounded/infinite stream
  refuses with over-cap, not timeout/OOM);
* a silent child is stopped by a finite timeout;
* nonzero exit and spawn failure refuse with fixed vocabulary;
* stderr (and stdout bodies) are never echoed into the refusal;
* termination kills only the adapter's own subprocess group: descendants
  of the managed child are stopped, an unrelated test child survives.

Each call runs the real ``_command`` under a minimal SIGALRM outer watchdog
(main test thread; the suite is single-threaded): if the alarm fires inside
``_command`` it raises here and unwinds through ``_command``'s own finally,
which kills the managed process group. No threads, no wrappers to join.
"""

import contextlib
import io
import os
import signal
import stat
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import bot_promotion
from bot_promotion import PromotionError

OUTER_WATCHDOG = 20  # seconds; SIGALRM takes an int


def run_guarded(argv, *, cap, timeout):
    """Run real _command with a SIGALRM outer watchdog so hangs fail fast."""
    def _expired(signum, frame):
        raise AssertionError(
            "managed command did not settle within outer watchdog "
            f"{OUTER_WATCHDOG}s (hang)"
        )
    previous = signal.signal(signal.SIGALRM, _expired)
    signal.alarm(OUTER_WATCHDOG)
    try:
        return bot_promotion._command(argv, cap=cap, timeout=timeout)
    finally:
        signal.alarm(0)
        signal.signal(signal.SIGALRM, previous)


def write_fake(directory, name, body):
    path = Path(directory) / name
    path.write_text("#!/usr/bin/env python3\n" + body, encoding="utf-8")
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
    return str(path)


def pid_alive(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


class CommandAdapterTest(unittest.TestCase):
    def test_success_small_local_child(self):
        with tempfile.TemporaryDirectory() as tmp:
            fake = write_fake(
                tmp, "ok_child.py", "import sys; sys.stdout.buffer.write(b'ok-123')\n"
            )
            out = run_guarded([sys.executable, fake], cap=65536, timeout=10)
            self.assertEqual(out, b"ok-123")

    def test_output_over_cap_refuses_during_reading(self):
        # Finite-but-large burst: must refuse with over-cap well before the
        # adapter timeout, proving the cap is checked while streaming.
        cap = 1024
        with tempfile.TemporaryDirectory() as tmp:
            fake = write_fake(
                tmp,
                "burst_child.py",
                "import sys; sys.stdout.buffer.write(b'x' * 1048576)\n",
            )
            start = time.monotonic()
            with self.assertRaises(PromotionError) as ctx:
                run_guarded([sys.executable, fake], cap=cap, timeout=10)
            elapsed = time.monotonic() - start
            self.assertIn("output over cap", str(ctx.exception))
            self.assertLess(elapsed, 10, "over-cap must fire during reading")

    def test_infinite_stream_refuses_over_cap_not_timeout(self):
        # communicate()-after-capture would hang until timeout (or OOM);
        # streaming enforcement refuses with over-cap instead. The fixture
        # is throttled (~3 MiB/s, ~32 MiB over the outer watchdog) so even a
        # regressed capture-the-whole-stream adapter cannot OOM CI before
        # the watchdog fires; the 4 KiB cap still trips on the first chunk.
        cap = 4096
        with tempfile.TemporaryDirectory() as tmp:
            fake = write_fake(
                tmp,
                "infinite_child.py",
                "import sys, time\n"
                "chunk = b'y' * 65536\n"
                "while True:\n"
                "    sys.stdout.buffer.write(chunk)\n"
                "    sys.stdout.buffer.flush()\n"
                "    time.sleep(0.02)\n",
            )
            start = time.monotonic()
            with self.assertRaises(PromotionError) as ctx:
                run_guarded([sys.executable, fake], cap=cap, timeout=10)
            elapsed = time.monotonic() - start
            self.assertIn("output over cap", str(ctx.exception))
            self.assertNotIn("timed out", str(ctx.exception))
            self.assertLess(elapsed, 10, "infinite stream must be cut during reading")

    def test_finite_timeout_stops_silent_child(self):
        with tempfile.TemporaryDirectory() as tmp:
            fake = write_fake(
                tmp, "sleep_child.py", "import time; time.sleep(30)\n"
            )
            start = time.monotonic()
            with self.assertRaises(PromotionError) as ctx:
                run_guarded([sys.executable, fake], cap=65536, timeout=2)
            elapsed = time.monotonic() - start
            self.assertIn("timed out", str(ctx.exception))
            self.assertLess(elapsed, OUTER_WATCHDOG, "timeout must be finite")

    def test_nonzero_exit_refuses_fixed_vocabulary(self):
        with tempfile.TemporaryDirectory() as tmp:
            fake = write_fake(
                tmp,
                "refuse_child.py",
                "import sys; sys.stdout.buffer.write(b'body-bytes'); "
                "sys.exit(3)\n",
            )
            with self.assertRaises(PromotionError) as ctx:
                run_guarded([sys.executable, fake], cap=65536, timeout=10)
            message = str(ctx.exception)
            self.assertIn("refused", message)
            self.assertNotIn("body-bytes", message)

    def test_spawn_failure_refuses_unavailable(self):
        with self.assertRaises(PromotionError) as ctx:
            run_guarded(
                ["/nonexistent-bot-promotion-probe-xyz/false"],
                cap=65536,
                timeout=10,
            )
        self.assertIn("unavailable", str(ctx.exception))

    def test_stderr_never_echoed(self):
        # Capture Python-level stdout/stderr around the real _command: a
        # direct print/logging leak would pass an exception-text-only check.
        sentinel = "stderr-secret-" + "q" * 12
        with tempfile.TemporaryDirectory() as tmp:
            fake = write_fake(
                tmp,
                "noisy_child.py",
                "import sys; "
                f"sys.stderr.write({sentinel!r}); "
                "sys.stdout.buffer.write(b'stdout-body'); "
                "sys.exit(1)\n",
            )
            out_buf, err_buf = io.StringIO(), io.StringIO()
            with contextlib.redirect_stdout(out_buf), contextlib.redirect_stderr(err_buf):
                with self.assertRaises(PromotionError) as ctx:
                    run_guarded([sys.executable, fake], cap=65536, timeout=10)
            message = str(ctx.exception)
            self.assertNotIn(sentinel, message)
            self.assertNotIn("stdout-body", message)
            self.assertNotIn(sentinel, out_buf.getvalue())
            self.assertNotIn(sentinel, err_buf.getvalue())
            self.assertNotIn("stdout-body", out_buf.getvalue())
            self.assertIn("refused", message)

    def test_terminate_only_own_subprocess_group(self):
        with tempfile.TemporaryDirectory() as tmp:
            pid_file = os.path.join(tmp, "grandchild.pid")
            fake = write_fake(
                tmp,
                "group_child.py",
                "import subprocess, sys, time\n"
                "g = subprocess.Popen([sys.executable, '-c', "
                "'import time; time.sleep(30)'])\n"
                f"open({pid_file!r}, 'w').write(str(g.pid))\n"
                "time.sleep(30)\n",
            )
            # Unrelated child in the test's own group; must survive killpg.
            unrelated = subprocess.Popen(
                [sys.executable, "-c", "import time; time.sleep(30)"],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                stdin=subprocess.DEVNULL,
            )
            try:
                self.assertIsNone(unrelated.poll())
                with self.assertRaises(PromotionError) as ctx:
                    run_guarded([sys.executable, fake], cap=65536, timeout=2)
                self.assertIn("timed out", str(ctx.exception))
                with open(pid_file, encoding="utf-8") as handle:
                    grandchild_pid = int(handle.read().strip())
                # Descendant must be reaped by the adapter's group kill.
                deadline = time.monotonic() + 5.0
                while pid_alive(grandchild_pid) and time.monotonic() < deadline:
                    time.sleep(0.05)
                self.assertFalse(
                    pid_alive(grandchild_pid),
                    "managed descendant must be stopped by group kill",
                )
                # Unrelated child shares neither session nor group: survives.
                self.assertIsNone(
                    unrelated.poll(), "unrelated child must survive group kill"
                )
            finally:
                if unrelated.poll() is None:
                    unrelated.terminate()
                    try:
                        unrelated.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        unrelated.kill()
                        unrelated.wait(timeout=5)


class _SmallCapGitHub(bot_promotion._GhApiGitHub):
    """Test-only small caps/timeouts; production defaults stay untouched."""

    TIMEOUT_SECONDS = 2
    MAX_JSON_BYTES = 1024
    MAX_ZIP_BYTES = 1024


JSON_RUN_ROUTE = "repos/TogetherWeOwn/two-bot-next/actions/runs/123"
ZIP_ROUTE = "repos/TogetherWeOwn/two-bot-next/actions/artifacts/456/zip"


def write_fake_gh(directory, body):
    return write_fake(directory, "gh", body)


@contextlib.contextmanager
def fake_gh_on_path(directory):
    # The child never receives the run's broker or provider credentials.
    clean = {"PATH": directory + os.pathsep + os.environ.get("PATH", ""),
             "HOME": directory, "LANG": "C.UTF-8"}
    with mock.patch.dict(os.environ, clean, clear=True):
        yield


def run_gh_guarded(func):
    """Run a native _GhApiGitHub call under the SIGALRM outer watchdog."""
    def _expired(signum, frame):
        raise AssertionError(
            "managed github read did not settle within outer watchdog "
            f"{OUTER_WATCHDOG}s (hang)"
        )
    previous = signal.signal(signal.SIGALRM, _expired)
    signal.alarm(OUTER_WATCHDOG)
    try:
        return func()
    finally:
        signal.alarm(0)
        signal.signal(signal.SIGALRM, previous)


class GhApiGitHubNativeTest(unittest.TestCase):
    """Native bot_promotion._GhApiGitHub wiring through a local fake gh.

    Real ``_GhApiGitHub.get_json``/``get_bytes`` with a ``gh`` executable
    faked on PATH (bounded local Python child, no broker/network). Only
    PATH and the small test-specific class caps/timeouts above are patched;
    ``_run``/``_command`` are never stubbed. Breaks caught: ``cap=cap``
    replaced with a huge cap (over-cap tests succeed instead of refusing),
    and omitted timeout forwarding (timeout tests exceed the small class
    timeout and hit the outer watchdog).
    """

    def test_get_json_positive_small_local_gh(self):
        with tempfile.TemporaryDirectory() as tmp:
            write_fake_gh(
                tmp, "import sys; sys.stdout.buffer.write(b'{\"ok\":true}')\n"
            )
            with fake_gh_on_path(tmp):
                out = run_gh_guarded(
                    lambda: _SmallCapGitHub().get_json(JSON_RUN_ROUTE)
                )
            self.assertEqual(out, {"ok": True})

    def test_get_bytes_positive_small_local_gh(self):
        with tempfile.TemporaryDirectory() as tmp:
            write_fake_gh(
                tmp, "import sys; sys.stdout.buffer.write(b'zip-ok-123')\n"
            )
            with fake_gh_on_path(tmp):
                out = run_gh_guarded(
                    lambda: _SmallCapGitHub().get_bytes(ZIP_ROUTE)
                )
            self.assertEqual(out, b"zip-ok-123")

    def test_get_json_over_cap_refuses_during_reading(self):
        # Valid JSON (~5 KiB) over the 1 KiB test cap but far under any
        # huge cap: cap=cap -> huge cap would parse and return it.
        with tempfile.TemporaryDirectory() as tmp:
            write_fake_gh(
                tmp,
                "import sys; sys.stdout.buffer.write(b'{\"data\":\"' "
                "+ b'A' * 5000 + b'\"}')\n",
            )
            with fake_gh_on_path(tmp):
                start = time.monotonic()
                with self.assertRaises(PromotionError) as ctx:
                    run_gh_guarded(
                        lambda: _SmallCapGitHub().get_json(JSON_RUN_ROUTE)
                    )
                elapsed = time.monotonic() - start
            self.assertIn("output over cap", str(ctx.exception))
            self.assertNotIn("timed out", str(ctx.exception))
            self.assertLess(elapsed, 10, "over-cap must fire during reading")

    def test_get_bytes_over_cap_refuses_during_reading(self):
        # Bounded 8 KiB burst over the 1 KiB test cap, far under a huge cap.
        with tempfile.TemporaryDirectory() as tmp:
            write_fake_gh(
                tmp, "import sys; sys.stdout.buffer.write(b'B' * 8192)\n"
            )
            with fake_gh_on_path(tmp):
                start = time.monotonic()
                with self.assertRaises(PromotionError) as ctx:
                    run_gh_guarded(
                        lambda: _SmallCapGitHub().get_bytes(ZIP_ROUTE)
                    )
                elapsed = time.monotonic() - start
            self.assertIn("output over cap", str(ctx.exception))
            self.assertNotIn("timed out", str(ctx.exception))
            self.assertLess(elapsed, 10, "over-cap must fire during reading")

    def test_get_json_timeout_forwards_class_timeout(self):
        # TIMEOUT_SECONDS=2: omitting timeout forwarding (default 60)
        # would hang past 10s into the outer watchdog instead.
        with tempfile.TemporaryDirectory() as tmp:
            write_fake_gh(tmp, "import time; time.sleep(30)\n")
            with fake_gh_on_path(tmp):
                start = time.monotonic()
                with self.assertRaises(PromotionError) as ctx:
                    run_gh_guarded(
                        lambda: _SmallCapGitHub().get_json(JSON_RUN_ROUTE)
                    )
                elapsed = time.monotonic() - start
            self.assertIn("timed out", str(ctx.exception))
            self.assertLess(elapsed, 10, "class timeout must be forwarded")

    def test_get_bytes_timeout_forwards_class_timeout(self):
        with tempfile.TemporaryDirectory() as tmp:
            write_fake_gh(tmp, "import time; time.sleep(30)\n")
            with fake_gh_on_path(tmp):
                start = time.monotonic()
                with self.assertRaises(PromotionError) as ctx:
                    run_gh_guarded(
                        lambda: _SmallCapGitHub().get_bytes(ZIP_ROUTE)
                    )
                elapsed = time.monotonic() - start
            self.assertIn("timed out", str(ctx.exception))
            self.assertLess(elapsed, 10, "class timeout must be forwarded")

    def test_get_json_nonzero_refuses_fixed_vocabulary_no_leak(self):
        sentinel = "gh-json-secret-" + "q" * 12
        with tempfile.TemporaryDirectory() as tmp:
            write_fake_gh(
                tmp,
                "import sys; "
                f"sys.stderr.write({sentinel!r}); "
                "sys.stdout.buffer.write(b'json-body-bytes'); "
                "sys.exit(3)\n",
            )
            with fake_gh_on_path(tmp):
                out_buf, err_buf = io.StringIO(), io.StringIO()
                with contextlib.redirect_stdout(out_buf), contextlib.redirect_stderr(err_buf):
                    with self.assertRaises(PromotionError) as ctx:
                        run_gh_guarded(
                            lambda: _SmallCapGitHub().get_json(JSON_RUN_ROUTE)
                        )
            message = str(ctx.exception)
            self.assertIn("refused", message)
            self.assertNotIn("json-body-bytes", message)
            self.assertNotIn(sentinel, message)
            self.assertNotIn(sentinel, out_buf.getvalue())
            self.assertNotIn(sentinel, err_buf.getvalue())
            self.assertNotIn("json-body-bytes", out_buf.getvalue())

    def test_get_bytes_nonzero_refuses_fixed_vocabulary_no_leak(self):
        sentinel = "gh-bytes-secret-" + "r" * 12
        with tempfile.TemporaryDirectory() as tmp:
            write_fake_gh(
                tmp,
                "import sys; "
                f"sys.stderr.write({sentinel!r}); "
                "sys.stdout.buffer.write(b'bytes-body-bytes'); "
                "sys.exit(1)\n",
            )
            with fake_gh_on_path(tmp):
                out_buf, err_buf = io.StringIO(), io.StringIO()
                with contextlib.redirect_stdout(out_buf), contextlib.redirect_stderr(err_buf):
                    with self.assertRaises(PromotionError) as ctx:
                        run_gh_guarded(
                            lambda: _SmallCapGitHub().get_bytes(ZIP_ROUTE)
                        )
            message = str(ctx.exception)
            self.assertIn("refused", message)
            self.assertNotIn("bytes-body-bytes", message)
            self.assertNotIn(sentinel, message)
            self.assertNotIn(sentinel, out_buf.getvalue())
            self.assertNotIn(sentinel, err_buf.getvalue())
            self.assertNotIn("bytes-body-bytes", out_buf.getvalue())


if __name__ == "__main__":
    unittest.main()
