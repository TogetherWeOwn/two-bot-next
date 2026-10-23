"""Offline behavioral acceptance for the required nextest lane's cache fallback.

Execute the workflow's actual preflight, pinned installer and statistics shells.
The stateful double models sccache 0.18.0's important distinction: cold
--show-stats succeeds without starting a daemon or contacting its backend;
--zero-stats connects/starts and validates it, including an existing daemon.
No network, credentials, Rust compilation or additional dependencies are used
(PyYAML is already required by the workflow-policy suite).

Run: PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts/ci -p 'test_nextest_cache_outage_acceptance.py' -v

Protected acceptance file: implementation owners integrate it verbatim;
test corrections belong to the independent test author.
"""

import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import unittest

import yaml


ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = ROOT / ".github/workflows/reusable-rust-nextest.yml"
PREFLIGHT = "Verify sccache availability (uncached fallback)"
INSTALL = "Install pinned cargo-nextest"
STATS = "sccache stats (advisory speed evidence)"
ACTION_PIN = "mozilla-actions/sccache-action@fc920bf0ec8de6ee65d409111f7ec508035751ba"
STDOUT_DIAGNOSTIC = "SYNTHETIC_CACHE_STDOUT_DIAGNOSTIC"
STDERR_DIAGNOSTIC = "SYNTHETIC_CACHE_STDERR_DIAGNOSTIC"

# One executable body, installed under three names. Trace files expose actual
# cross-process ordering and the environment seen by Cargo and its compiler.
DOUBLE = r'''
import json
import os
from pathlib import Path
import sys

root = Path(os.environ["FIXTURE_ROOT"])
name = Path(sys.argv[0]).name
args = sys.argv[1:]
daemon = root / "daemon"

def record(event, **fields):
    with (root / "trace.jsonl").open("a") as trace:
        trace.write(json.dumps(dict(event=event, tool=name, args=args,
                                   wrapper=os.environ.get("RUSTC_WRAPPER"),
                                   phase=os.environ["FIXTURE_PHASE"], **fields)) + "\n")

def diagnostic():
    print("SYNTHETIC_CACHE_STDOUT_DIAGNOSTIC")
    print("SYNTHETIC_CACHE_STDERR_DIAGNOSTIC", file=sys.stderr)

def connect():
    if daemon.exists():
        return True
    record("backend_start")
    if os.environ["FIXTURE_BACKEND"] == "outage":
        diagnostic()
        return False
    daemon.write_text("running")
    record("daemon_started")
    return True

record("invoke")
if name == "sccache":
    if args == ["--show-stats"]:
        # A cold stats query is false readiness, not a backend health probe.
        if os.environ["FIXTURE_STATS"] == "fail":
            diagnostic()
            sys.exit(2)
        print("Compile requests 3\nCache hits 2" if daemon.exists()
              else "Compile requests 0\nCache hits 0")
        sys.exit(0)
    if args in (["--zero-stats"], ["--start-server"]):
        if args == ["--start-server"] and daemon.exists():
            sys.exit(2)  # Not equivalent to connecting to a running daemon.
        sys.exit(0 if connect() else 2)
    if args and args[0] == str(root / "bin/rustc"):
        if not connect():
            sys.exit(2)
        os.execv(args[0], args)
    sys.exit(64)
if name == "cargo":
    # Exercise the compiler boundary instead of merely inspecting GITHUB_ENV.
    compiler = str(root / "bin/rustc")
    command = [compiler, "--version"]
    if os.environ.get("RUSTC_WRAPPER"):
        command.insert(0, os.environ["RUSTC_WRAPPER"])
    os.execvpe(command[0], command, os.environ)
if name == "rustc":
    sys.exit(int(os.environ["FIXTURE_COMPILER_EXIT"]))
sys.exit(64)
'''


class NextestCacheOutageAcceptanceTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.job = yaml.safe_load(WORKFLOW.read_text())["jobs"]["nextest"]
        cls.steps = cls.job["steps"]
        cls.by_name = {step.get("name"): step for step in cls.steps}

    def setUp(self):
        parent = (os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")
                  or os.environ.get("PAPERCLIP_SCRATCH_DIR"))
        scratch = tempfile.TemporaryDirectory(prefix="cache-acceptance-", dir=parent)
        self.addCleanup(scratch.cleanup)
        self.root = Path(scratch.name)
        (self.root / "bin").mkdir()
        for tool in ("sccache", "cargo", "rustc"):
            path = self.root / "bin" / tool
            path.write_text(f"#!{sys.executable}\n" + DOUBLE)
            path.chmod(0o755)
        (self.root / "github.env").touch()
        self.env = {
            "PATH": str(self.root / "bin") + os.pathsep + os.defpath,
            "HOME": str(self.root),
            "GITHUB_ENV": str(self.root / "github.env"),
            "RUSTC_WRAPPER": self.job["env"]["RUSTC_WRAPPER"],
            "FIXTURE_ROOT": str(self.root),
            "FIXTURE_BACKEND": "healthy",
            "FIXTURE_STATS": "ok",
            "FIXTURE_COMPILER_EXIT": "0",
        }
        self.assertEqual(self.env["RUSTC_WRAPPER"], "sccache")

    def trace(self, event="invoke"):
        path = self.root / "trace.jsonl"
        rows = [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []
        return [row for row in rows if row["event"] == event]

    def run_step(self, name):
        step = self.by_name[name]
        self.assertEqual(step["shell"], "bash")
        env = dict(self.env, FIXTURE_PHASE=name)
        env.update(step.get("env", {}))
        return subprocess.run(
            [shutil.which("bash"), "--noprofile", "--norc", "-e", "-o", "pipefail", "-c", step["run"]],
            cwd=self.root, env=env, capture_output=True, text=True, timeout=10,
        )

    def apply_github_env(self):
        # Actions passes file values literally to a new step, not via `source`.
        for line in (self.root / "github.env").read_text().splitlines():
            key, separator, value = line.partition("=")
            self.assertEqual(separator, "=", "malformed environment-file assignment")
            self.env[key] = value

    def assert_redacted(self, result):
        for diagnostic in (STDOUT_DIAGNOSTIC, STDERR_DIAGNOSTIC):
            self.assertNotIn(diagnostic, result.stdout + result.stderr,
                             "cache diagnostics must not escape the workflow shell")

    def preflight_then_install(self):
        preflight = self.run_step(PREFLIGHT)
        self.assertEqual(preflight.returncode, 0, "cache failure must not fail preflight")
        self.assert_redacted(preflight)
        self.apply_github_env()
        return preflight, self.run_step(INSTALL)

    def assert_compiler_wrapper(self, expected):
        compilers = [row for row in self.trace() if row["tool"] == "rustc"]
        self.assertEqual(len(compilers), 1, "the actual installer must reach its compiler")
        self.assertEqual(compilers[0]["wrapper"], expected)
        cargo = [row for row in self.trace() if row["tool"] == "cargo"]
        self.assertEqual(len(cargo), 1)
        self.assertEqual(cargo[0]["args"], ["install", "cargo-nextest", "--version", "0.9.99", "--locked"])
        self.assertEqual(cargo[0]["wrapper"], expected)

    def test_cold_remote_outage_falls_back_before_install(self):
        self.env["FIXTURE_BACKEND"] = "outage"
        preflight, install = self.preflight_then_install()
        self.assertEqual(install.returncode, 0,
                         "cold stats can report ready while backend startup fails; installer must run uncached")
        self.assert_compiler_wrapper("")
        self.assertIn("::warning::", preflight.stdout)
        self.assertFalse((self.root / "daemon").exists())
        attempts = self.trace("backend_start")
        self.assertEqual(len(attempts), 1)
        self.assertEqual(attempts[0]["phase"], PREFLIGHT)
        self.assertFalse(any(row["tool"] == "sccache" and row["phase"] == INSTALL
                             for row in self.trace()), "fallback must bypass the wrapper entirely")

    def test_healthy_cold_cache_stays_enabled(self):
        preflight, install = self.preflight_then_install()
        self.assertEqual(install.returncode, 0)
        self.assert_compiler_wrapper("sccache")
        self.assertNotIn("::warning::", preflight.stdout)
        starts = self.trace("daemon_started")
        self.assertEqual(len(starts), 1)
        self.assertEqual(starts[0]["phase"], PREFLIGHT,
                         "the cold daemon must be validated before Cargo starts")

    def test_healthy_already_running_cache_stays_enabled(self):
        (self.root / "daemon").write_text("already running")
        preflight, install = self.preflight_then_install()
        self.assertEqual(install.returncode, 0)
        self.assert_compiler_wrapper("sccache")
        self.assertNotIn("::warning::", preflight.stdout)
        self.assertEqual(self.trace("backend_start"), [])
        self.assertTrue(any(row["tool"] == "sccache" and row["phase"] == PREFLIGHT
                            for row in self.trace()), "availability must still be checked")

    def test_compiler_exit_37_remains_fatal_after_fallback(self):
        self.assertFalse(self.by_name[INSTALL].get("continue-on-error", False))
        self.env.update(FIXTURE_BACKEND="outage", FIXTURE_COMPILER_EXIT="37")
        _, install = self.preflight_then_install()
        self.assertEqual(install.returncode, 37,
                         "cache fallback must preserve the compiler/install failure")
        self.assert_compiler_wrapper("")

    def test_failed_statistics_are_advisory_and_redacted(self):
        self.env["FIXTURE_STATS"] = "fail"
        stats = self.run_step(STATS)
        self.assertEqual(stats.returncode, 0, "statistics are not a compiler/test gate")
        self.assert_redacted(stats)
        self.assertIn("::warning::", stats.stdout)
        self.assertEqual([row["args"] for row in self.trace()], [["--show-stats"]])

    def test_healthy_statistics_remain_visible(self):
        (self.root / "daemon").write_text("running")
        stats = self.run_step(STATS)
        self.assertEqual(stats.returncode, 0)
        self.assertIn("Compile requests 3", stats.stdout)
        self.assertIn("Cache hits 2", stats.stdout)
        self.assertNotIn("::warning::", stats.stdout)

    def test_pinned_action_disables_post_reporting_before_first_cargo(self):
        action = self.by_name["Start sccache"]
        self.assertEqual(action["uses"], ACTION_PIN)
        self.assertIs(action["with"]["disable_annotations"], True,
                      "the action post hook must not turn cache outages into job failures")
        action_index = self.steps.index(action)
        preflight_index = self.steps.index(self.by_name[PREFLIGHT])
        first_cargo = next(index for index, step in enumerate(self.steps)
                           if re.search(r"\bcargo(?:\s|$)",
                                        step.get("run", "")))
        self.assertLess(action_index, preflight_index)
        self.assertLess(preflight_index, first_cargo)
        self.assertEqual(self.steps[first_cargo]["name"], INSTALL)


if __name__ == "__main__":
    unittest.main()
