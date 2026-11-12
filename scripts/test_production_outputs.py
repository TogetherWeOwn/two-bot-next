"""Offline deploy-ID output fixtures with a stubbed producer."""

import io
import os
import subprocess
import sys
import tempfile
import time
import unittest
from contextlib import redirect_stderr
from pathlib import Path
from unittest.mock import MagicMock, patch

sys.path.insert(0, str(Path(__file__).resolve().parent))
import production_outputs

SCRIPT = Path(production_outputs.__file__).resolve()
SHA = "abcdef0123456789abcdef0123456789abcdef01"
SHA7 = SHA[:7]
GOOD_ID = f"2026.10.21.3+{SHA7}"
OTHER_SHA = "0123456789abcdef0123456789abcdef01234567"


class Harness(unittest.TestCase):
    def setUp(self):
        holder = tempfile.TemporaryDirectory()
        self.addCleanup(holder.cleanup)
        self.temp = Path(holder.name).resolve()
        self.out = self.temp / "output"
        self.summary = self.temp / "summary"
        environment = patch.dict(os.environ, {}, clear=True)
        environment.start()
        self.addCleanup(environment.stop)

    def env(self, **extra):
        base = {"PATH": os.environ.get("PATH", ""), "RUNNER_TEMP": str(self.temp),
                "GITHUB_OUTPUT": str(self.out), "GITHUB_STEP_SUMMARY": str(self.summary)}
        base.update(extra)
        return base

    def output(self):
        return self.out.read_text() if self.out.exists() else ""


class DeployIdTests(Harness):
    def run_main(self, completed=None, raises=None, sha=SHA):
        if completed is not None and completed.returncode:
            raises = production_outputs.OutputError("deploy ID producer failed")
        stub = patch.object(production_outputs, "capture_deploy_id",
                            side_effect=raises,
                            return_value=completed.stdout if completed is not None else None)
        with stub as run, redirect_stderr(io.StringIO()) as err, \
                patch.dict(os.environ, self.env()):
            code = production_outputs.main(["deploy-id", "--sha", sha])
        return code, err.getvalue(), run

    def done(self, stdout, code=0):
        return subprocess.CompletedProcess([], code, stdout=stdout, stderr=b"")

    def test_accepts_with_and_without_newline_and_calls_production(self):
        for stdout in (f"DEPLOY_ID={GOOD_ID}\n".encode(), f"DEPLOY_ID={GOOD_ID}".encode()):
            self.out.write_text("")
            code, _err, run = self.run_main(self.done(stdout))
            self.assertEqual(code, 0)
            self.assertEqual(self.output(), f"deploy_id={GOOD_ID}\n")
        argv = run.call_args.args[0]
        self.assertEqual(argv[0], sys.executable)
        self.assertEqual(argv[1:], [str(SCRIPT.with_name("deploy_id.py")), "next",
                                    "--environment", "production", "--sha", SHA])
        self.assertEqual(run.call_args.kwargs, {})

    def test_rejects_malformed_producer_output(self):
        bad = [
            b"", b"\n", b"DEPLOY_ID=\n", f"DEPLOY_ID={GOOD_ID}\n\n".encode(),
            f"DEPLOY_ID={GOOD_ID}\nDEPLOY_ID={GOOD_ID}\n".encode(),
            f"DEPLOY_ID={GOOD_ID}\r\n".encode(), f"DEPLOY_ID={GOOD_ID}\x00".encode(),
            f"DEPLOY_ID={GOOD_ID}\x1b[0m\n".encode(), f"DEPLOY_ID={GOOD_ID} \n".encode(),
            f"deploy_id={GOOD_ID}\n".encode(), f" DEPLOY_ID={GOOD_ID}\n".encode(),
            f"noise\nDEPLOY_ID={GOOD_ID}\n".encode(), f"DEPLOY_ID={GOOD_ID}\nextra\n".encode(),
            b"DEPLOY_ID=\xff\xfe\n", f"DEPLOY_ID={GOOD_ID}\xc3\xa9\n".encode("latin-1"),
            f"DEPLOY_ID={GOOD_ID}=x\n".encode(),
            f"DEPLOY_ID=2026.13.01.1+{SHA7}\n".encode(),
            f"DEPLOY_ID=2026.02.30.1+{SHA7}\n".encode(),
            f"DEPLOY_ID=2026.10.21.0+{SHA7}\n".encode(),
            f"DEPLOY_ID=2026.10.21.03+{SHA7}\n".encode(),
            f"DEPLOY_ID=2026.10.21.1234567+{SHA7}\n".encode(),
            f"DEPLOY_ID=2026.10.21.3+{OTHER_SHA[:7]}\n".encode(),
            f"DEPLOY_ID=2026.10.21.3+{SHA7.upper()}\n".encode(),
            f"DEPLOY_ID=2026.10.21.3+{SHA[:8]}\n".encode(),
            f"DEPLOY_ID=2026.10.21.3+{SHA[:6]}\n".encode(),
            b"DEPLOY_ID=" + b"1" * 5000 + b"\n",
        ]
        for stdout in bad:
            with self.subTest(stdout=stdout[:60]):
                code, err, _run = self.run_main(self.done(stdout))
                self.assertEqual(code, 1)
                self.assertEqual(self.output(), "")
                self.assertEqual(err.count("\n"), 1)
                self.assertNotIn("\x00", err)

    def test_failed_or_unavailable_producer_writes_nothing(self):
        code, _err, _run = self.run_main(self.done(f"DEPLOY_ID={GOOD_ID}\n".encode(), code=1))
        self.assertEqual((code, self.output()), (1, ""))
        code, _err, _run = self.run_main(raises=OSError("boom"))
        self.assertEqual((code, self.output()), (1, ""))
        code, _err, _run = self.run_main(raises=subprocess.TimeoutExpired("x", 1))
        self.assertEqual((code, self.output()), (1, ""))

    def test_rejects_bad_sha_without_running_producer(self):
        for sha in ("", "abc", SHA.upper(), SHA + "0", "g" * 40):
            with self.subTest(sha=sha):
                code, _err, run = self.run_main(self.done(b""), sha=sha)
                self.assertEqual(code, 1)
                run.assert_not_called()

    def test_stub_producer_failure_writes_nothing(self):
        stub = self.temp / "producer.py"
        stub.write_text("raise SystemExit(1)\n")
        with patch.object(production_outputs, "DEPLOY_ID_SCRIPT", stub), \
                patch.dict(os.environ, self.env()), redirect_stderr(io.StringIO()):
            self.assertEqual(production_outputs.main(["deploy-id", "--sha", SHA]), 1)
        self.assertEqual(self.output(), "")

    def test_bounded_producer_transport(self):
        def command(source):
            return [sys.executable, "-c", source]

        good = f"DEPLOY_ID={GOOD_ID}\n".encode()
        self.assertEqual(production_outputs.capture_deploy_id(command(
            f"import os; os.write(2, b'x' * 1000000); os.write(1, {good!r})")), good)
        for source in ("import os; os.write(1, b'x' * 10000)",
                       "import sys; sys.exit(1)"):
            with self.subTest(source=source), self.assertRaises(production_outputs.OutputError):
                production_outputs.capture_deploy_id(command(source))
        with patch.object(production_outputs, "DEPLOY_ID_TIMEOUT", 0.1), \
                self.assertRaises(subprocess.TimeoutExpired):
            production_outputs.capture_deploy_id(command("import time; time.sleep(10)"))

    def test_signal_precedes_reap_on_normal_completion(self):
        process, selector, calls = MagicMock(), MagicMock(), MagicMock()
        process.pid = 12345
        selector.__enter__.return_value.select.return_value = [True]
        calls.attach_mock(process.wait, "wait")
        with patch.object(production_outputs.subprocess, "Popen", return_value=process), \
                patch.object(production_outputs.selectors, "DefaultSelector", return_value=selector), \
                patch.object(production_outputs.os, "read", return_value=b""), \
                patch.object(production_outputs.os, "waitid", return_value=os.waitid_result((12345, 0, 0, 0, os.CLD_EXITED))) as waitid, \
                patch.object(production_outputs.os, "killpg") as killpg:
            calls.attach_mock(killpg, "killpg")
            self.assertEqual(production_outputs.capture_deploy_id(["producer"]), b"")
        waitid.assert_called_once_with(os.P_PID, process.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT)
        self.assertEqual(calls.mock_calls, [unittest.mock.call.killpg(process.pid, production_outputs.signal.SIGKILL), unittest.mock.call.wait(timeout=1)])

    def test_cleanup_timeout_refuses_without_output(self):
        process = MagicMock()
        process.pid = 12345
        process.wait.side_effect = subprocess.TimeoutExpired("producer", 1)
        selector = MagicMock()
        selector.__enter__.return_value.select.return_value = []
        with patch.object(production_outputs.subprocess, "Popen", return_value=process), \
                patch.object(production_outputs.os, "killpg") as killpg, \
                patch.object(production_outputs.selectors, "DefaultSelector", return_value=selector), \
                patch.dict(os.environ, self.env()), redirect_stderr(io.StringIO()) as err:
            code = production_outputs.main(["deploy-id", "--sha", SHA])
        self.assertEqual(code, 1)
        self.assertEqual(self.output(), "")
        self.assertEqual(err.getvalue(), "production_outputs failed: deploy ID producer unavailable\n")
        killpg.assert_called_once_with(process.pid, production_outputs.signal.SIGKILL)
        process.wait.assert_called_once_with(timeout=1)
        process.stdout.close.assert_called_once_with()

    def test_timed_out_direct_child_is_reaped(self):
        popen = subprocess.Popen
        children = []

        def spawn(*args, **kwargs):
            child = popen(*args, **kwargs)
            children.append(child)
            return child

        with patch.object(production_outputs.subprocess, "Popen", side_effect=spawn), \
                patch.object(production_outputs, "DEPLOY_ID_TIMEOUT", 0.1), \
                self.assertRaises(subprocess.TimeoutExpired):
            production_outputs.capture_deploy_id([sys.executable, "-c", "import time; time.sleep(10)"])
        self.assertEqual(len(children), 1)
        self.assertIsNotNone(children[0].returncode)
        self.assertTrue(children[0].stdout.closed)
        with self.assertRaises(ChildProcessError):
            os.waitpid(children[0].pid, os.WNOHANG)

    def test_timeout_kills_live_descendant_even_after_producer_exits(self):
        for mode in ("alive", "exited", "overflow"):
            ready, late = self.temp / f"ready-{mode}", self.temp / f"late-{mode}"
            child = (f"from pathlib import Path; import time; Path({str(ready)!r}).touch(); "
                     f"time.sleep(2); Path({str(late)!r}).touch()")
            parent = ("import subprocess,sys,time; from pathlib import Path; "
                      f"subprocess.Popen([sys.executable, '-c', {child!r}]); "
                      f"p=Path({str(ready)!r}); "
                      "exec('while not p.exists(): time.sleep(0.01)'); "
                      + {"exited": "sys.exit(0)", "alive": "time.sleep(10)",
                         "overflow": "sys.stdout.write('x'*10000); sys.stdout.flush(); time.sleep(10)"}[mode])
            error = production_outputs.OutputError if mode == "overflow" else subprocess.TimeoutExpired
            with self.subTest(mode=mode), \
                    patch.object(production_outputs, "DEPLOY_ID_TIMEOUT", 1), \
                    self.assertRaises(error):
                production_outputs.capture_deploy_id([sys.executable, "-c", parent])
            self.assertTrue(ready.exists())
            time.sleep(2.3)
            self.assertFalse(late.exists())

    def test_missing_output_path_refuses(self):
        with patch.dict(os.environ, self.env(GITHUB_OUTPUT="relative")), \
                redirect_stderr(io.StringIO()), \
                patch.object(production_outputs, "capture_deploy_id",
                             return_value=f"DEPLOY_ID={GOOD_ID}\n".encode()):
            self.assertEqual(production_outputs.main(["deploy-id", "--sha", SHA]), 1)


if __name__ == "__main__":
    unittest.main()
