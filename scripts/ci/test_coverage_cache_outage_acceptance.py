"""Execute coverage-shard's actual Bash with offline cache/compiler fixtures.

Deleting the GITHUB_ENV wrapper reset must break the following measurement
process. Making cache statistics mandatory must fail the advisory tests. The
whole measurement snippet runs unchanged, including cov_run and LCOV export;
only tool installation, cache, compiler and instrumented-build commands are
synthetic. No network, Rust compilation or external cache is used here.
"""

import io
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tarfile
import tempfile
import unittest

import yaml

ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = ROOT / ".github/workflows/check.yml"
LCOV = "TN:\nSF:crates/discord/src/executor.rs\nDA:1,1\nend_of_record\n"

# Every fixture executable records its invocation. Unknown commands/arguments
# fail closed instead of reaching a real compiler, cache service or network.
TOOLS = r'''
import json
import os
from pathlib import Path
import signal
import subprocess
import sys

name = Path(sys.argv[0]).name
args = sys.argv[1:]
with open(os.environ["TRACE"], "a") as log:
    log.write(json.dumps({"tool": name, "args": args,
                          "wrapper": os.environ.get("RUSTC_WRAPPER")}) + "\n")

if name == "timeout":
    assert args[0] in ("30s", "10s"), args
    hanging = ((args[0] == "30s" and os.environ["CACHE_MODE"] == "hang") or
               (args[0] == "10s" and os.environ["STATS_MODE"] == "hang"))
    # Keep the actual YAML timeout invocation and real timeout exit semantics,
    # but shorten only a deliberately hanging fixture's external watchdog.
    budget = "0.2s" if hanging else args[0]
    os.execv(os.environ["REAL_TIMEOUT"], ["timeout", budget, *args[1:]])
elif name == "sccache":
    mode = os.environ["STATS_MODE"] if args == ["--show-stats"] else os.environ["CACHE_MODE"]
    if mode == "hang":
        if args in (["--show-stats"], ["rustc", "--print", "cfg"]):
            signal.pause()
        # If a broken startup wrapper reaches compilation, fail immediately;
        # only the startup probe/statistics fixtures deliberately stall.
        sys.exit(86)
    if mode == "fail":
        sys.exit(86)
    assert mode == "healthy", mode
    if args == ["--show-stats"]:
        print("fixture cache hits: 1")
    else:
        assert args[0] == "rustc", args
        sys.exit(subprocess.call(args))
elif name == "rustc":
    if args == ["--print", "cfg"]:
        print('target_arch="fixture"')
    else:
        assert args == ["--fixture-compile"], args
        sys.exit(int(os.environ["COMPILER_STATUS"]))
elif name == "rustup":
    assert args == ["component", "add", "llvm-tools-preview"], args
elif name == "curl":
    assert args[:2] == ["-fsSL", "-o"] and len(args) == 4, args
    assert args[3].startswith("https://github.com/taiki-e/cargo-llvm-cov/releases/"), args
    Path(args[2]).write_bytes(Path(os.environ["FIXTURE_ARCHIVE"]).read_bytes())
elif name == "sha256sum":
    # The archive above is a local executable fixture, not the release asset.
    # Installation checksum behavior is outside these shell-control tests.
    assert args == ["--check", "-"], args
    assert sys.stdin.read().strip().endswith("cargo-llvm-cov.tar.gz")
elif name == "cargo-llvm-cov":
    assert args[0] == "llvm-cov", args
    if args[1] == "report":
        assert "--workspace" in args and "--lcov" in args, args
        status = int(os.environ["REPORT_STATUS"])
        if status:
            sys.exit(status)
        if os.environ["EMPTY_REPORT"] != "1":
            lcov = os.environ["FIXTURE_LCOV"]
            if "--output-path" in args:
                Path(args[args.index("--output-path") + 1]).write_text(lcov)
            else:
                sys.stdout.write(lcov)
    else:
        assert "--no-report" in args and "--test" in args, args
        compiler = ["rustc", "--fixture-compile"]
        wrapper = os.environ.get("RUSTC_WRAPPER")
        if wrapper:
            compiler.insert(0, wrapper)
        sys.exit(subprocess.call(compiler))
else:
    raise AssertionError("unexpected fixture command: " + name)
'''


class CoverageShell:
    def __init__(self, case, job, *, cache="healthy", stats="healthy",
                 compiler_status=0, report_status=0, empty_report=False):
        scratch = os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR") or os.environ.get("PAPERCLIP_SCRATCH_DIR")
        if scratch:
            Path(scratch).mkdir(parents=True, exist_ok=True)
        directory = tempfile.TemporaryDirectory(prefix="coverage-cache-shell-", dir=scratch)
        case.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.runner = self.root / "runner"
        self.runner.mkdir()
        self.env_file = self.root / "github-env"
        self.output_file = self.root / "github-output"
        self.trace_file = self.root / "trace.jsonl"
        for path in (self.env_file, self.output_file, self.trace_file):
            path.touch()
        executable = ("#!" + sys.executable + "\n" + TOOLS).encode()
        for command in ("timeout", "sccache", "rustc", "rustup", "curl", "sha256sum"):
            path = self.bin / command
            path.write_bytes(executable)
            path.chmod(0o755)
        archive = self.root / "fixture.tar.gz"
        with tarfile.open(archive, "w:gz") as tar:
            entry = tarfile.TarInfo("cargo-llvm-cov")
            entry.mode = 0o755
            entry.size = len(executable)
            tar.addfile(entry, io.BytesIO(executable))
        # Do not inherit any credentials, production configuration, compiler
        # wrappers or package-manager shims from the calling agent/runner.
        self.env = dict(job["env"]) | {
            "PATH": str(self.bin) + ":/usr/bin:/bin",
            "HOME": str(self.root),
            "GITHUB_WORKSPACE": str(self.root),
            "GITHUB_ENV": str(self.env_file),
            "GITHUB_OUTPUT": str(self.output_file),
            "RUNNER_TEMP": str(self.runner),
            "PAPERCLIP_RUN_SCRATCH_DIR": str(self.root / "scratch"),
            "TRACE": str(self.trace_file),
            "REAL_TIMEOUT": shutil.which("timeout", path="/usr/bin:/bin"),
            "FIXTURE_ARCHIVE": str(archive),
            "FIXTURE_LCOV": LCOV,
            "CACHE_MODE": cache,
            "STATS_MODE": stats,
            "COMPILER_STATUS": str(compiler_status),
            "REPORT_STATUS": str(report_status),
            "EMPTY_REPORT": "1" if empty_report else "0",
            "SHARD_LEGS": "discord-mutation_429_proves_no_effect_channel",
            "SHARD_INDEX": "0",
        }

    def run(self, step):
        # Match GitHub's explicit Bash shell: a new process per step, errexit
        # and pipefail enabled. No snippet reconstruction or copied shell logic.
        command = ["/bin/bash", "--noprofile", "--norc", "-e", "-o", "pipefail", "-c", step["run"]]
        with subprocess.Popen(command, cwd=self.root, env=self.env, text=True,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                              start_new_session=True) as process:
            try:
                stdout, stderr = process.communicate(timeout=5)
            except subprocess.TimeoutExpired:
                # A mutant that removes the stats watchdog must not leave a
                # stalled fixture child behind or hang the collected suite.
                os.killpg(process.pid, signal.SIGKILL)
                stdout, stderr = process.communicate()
                return subprocess.CompletedProcess(command, 124, stdout,
                                                   stderr + "fixture shell exceeded five seconds")
            return subprocess.CompletedProcess(command, process.returncode, stdout, stderr)

    def import_github_env(self):
        # Runner propagation, not `source`: KEY= replaces the job-level value
        # in a FOLLOWING subprocess, without editing the current shell env.
        for line in self.env_file.read_text().splitlines():
            key, value = line.split("=", 1)
            self.env[key] = value

    def output(self):
        return dict(line.split("=", 1) for line in self.output_file.read_text().splitlines())

    def trace(self, tool):
        return [row for line in self.trace_file.read_text().splitlines()
                if (row := json.loads(line))["tool"] == tool]


class CoverageCacheOutageAcceptance(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        workflow = yaml.load(WORKFLOW.read_text(), Loader=yaml.BaseLoader)
        cls.job = workflow["jobs"]["coverage-shard"]
        steps = {step.get("name"): step for step in cls.job["steps"]}
        cls.ready = steps["Check compiler cache availability"]
        cls.measure = steps["Patch coverage legs (shard instrumented run, no report)"]
        cls.stats = steps["sccache stats (advisory speed evidence)"]
        cls.install = steps["Start sccache"]

    def assert_success(self, result):
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def assert_measurement(self, shell, wrapper):
        result = shell.run(self.measure)
        self.assert_success(result)
        self.assertEqual((shell.root / "lcov-shard-0.info").read_text(), LCOV)
        compiler = [row for row in shell.trace("rustc") if row["args"] == ["--fixture-compile"]]
        self.assertEqual(len(compiler), 1, shell.trace_file.read_text())
        self.assertEqual(compiler[0]["wrapper"], wrapper)
        builds = [row for row in shell.trace("cargo-llvm-cov") if row["args"][1] != "report"]
        self.assertEqual(len(builds), 1)
        self.assertIn("mutation_429_proves_no_effect_channel", builds[0]["args"])

    def test_healthy_startup_preserves_wrapper_for_following_measurement(self):
        shell = CoverageShell(self, self.job)
        self.assert_success(shell.run(self.ready))
        self.assertEqual(shell.output(), {"enabled": "true"})
        shell.import_github_env()
        self.assert_measurement(shell, "sccache")
        self.assertEqual([row["args"] for row in shell.trace("sccache")], [
            ["rustc", "--print", "cfg"], ["rustc", "--fixture-compile"],
        ])

    def test_failed_or_timed_out_startup_unsets_wrapper_in_following_measurement(self):
        for mode in ("fail", "hang"):
            with self.subTest(cache=mode):
                shell = CoverageShell(self, self.job, cache=mode)
                result = shell.run(self.ready)
                self.assert_success(result)
                self.assertIn("::warning::", result.stdout)
                self.assertEqual(shell.output(), {"enabled": "false"})
                shell.import_github_env()
                self.assert_measurement(shell, "")
                self.assertEqual([row["args"] for row in shell.trace("timeout")], [
                    ["30s", "sccache", "rustc", "--print", "cfg"],
                ])
                # The following build must not invoke a known-unusable wrapper.
                self.assertFalse(any(row["args"] == ["rustc", "--fixture-compile"]
                                     for row in shell.trace("sccache")))

    def test_healthy_statistics_are_collected(self):
        shell = CoverageShell(self, self.job)
        self.assert_success(shell.run(self.ready))
        result = shell.run(self.stats)
        self.assert_success(result)
        self.assertIn("fixture cache hits: 1", result.stdout)
        self.assertEqual(shell.trace("timeout")[-1]["args"], ["10s", "sccache", "--show-stats"])

    def test_statistics_failure_or_timeout_is_advisory(self):
        for mode in ("fail", "hang"):
            with self.subTest(stats=mode):
                shell = CoverageShell(self, self.job, stats=mode)
                self.assert_success(shell.run(self.ready))
                result = shell.run(self.stats)
                self.assert_success(result)
                self.assertIn("::warning::", result.stdout)
                self.assertEqual(shell.trace("timeout")[-1]["args"], ["10s", "sccache", "--show-stats"])

    def test_compiler_failure_remains_fatal_with_or_without_cache(self):
        for mode in ("healthy", "fail"):
            with self.subTest(cache=mode):
                shell = CoverageShell(self, self.job, cache=mode, compiler_status=17)
                self.assert_success(shell.run(self.ready))
                shell.import_github_env()
                result = shell.run(self.measure)
                self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertIn("::error::patch coverage measurement failed", result.stderr)
                compiler = [row for row in shell.trace("rustc") if row["args"] == ["--fixture-compile"]]
                self.assertEqual(len(compiler), 1)
                self.assertFalse((shell.root / "lcov-shard-0.info").exists())
                self.assertFalse(any(row["args"][1] == "report" for row in shell.trace("cargo-llvm-cov")))

    def test_report_failure_remains_fatal_with_or_without_cache(self):
        for mode in ("healthy", "fail"):
            with self.subTest(cache=mode):
                shell = CoverageShell(self, self.job, cache=mode, report_status=23)
                self.assert_success(shell.run(self.ready))
                shell.import_github_env()
                result = shell.run(self.measure)
                self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertIn("::error::patch coverage measurement failed", result.stderr)
                self.assertEqual(sum(row["args"][1] == "report" for row in shell.trace("cargo-llvm-cov")), 1)
                self.assertFalse((shell.root / "lcov-shard-0.info").exists())

    def test_empty_report_remains_fatal_after_stdout_retry(self):
        shell = CoverageShell(self, self.job, cache="fail", empty_report=True)
        self.assert_success(shell.run(self.ready))
        shell.import_github_env()
        result = shell.run(self.measure)
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("::error::patch coverage measurement produced no lcov report", result.stdout)
        self.assertEqual(sum(row["args"][1] == "report" for row in shell.trace("cargo-llvm-cov")), 2)

    def test_cache_action_post_hook_cannot_restart_unavailable_service(self):
        # An independent post hook would restart the broken service after the
        # tested step. The action's documented switch disables that post hook.
        self.assertEqual(self.install["with"]["disable_annotations"], "true")


if __name__ == "__main__":
    unittest.main()
