import os
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path


WORKFLOW = Path(__file__).resolve().parents[1] / ".github/workflows/pipeline-benchmark.yml"
APT_COMMANDS = ("apt-get update -qq", "apt-get install -y -qq --no-install-recommends python3")


class PipelineBenchSetupTests(unittest.TestCase):
    def setUp(self):
        step = WORKFLOW.read_text().split("      - name: Job-container prerequisites\n", 1)[1]
        self.script = textwrap.dedent(step.split("        run: |\n", 1)[1].split("      - name:", 1)[0])
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.root = Path(scratch.name)
        self.workspace = self.root / "synthetic checkout"
        self.workspace.mkdir()
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.trace = self.root / "trace"
        self.stub("git", '''{ printf 'git'; printf ' <%s>' "$@"; printf '\\n'; } >> "$TRACE"''')
        self.stub("apt-get", 'printf "apt-get %s\n" "$*" >> "$TRACE"\n'
                             'if [ "$1" = "$APT_FAILURE" ]; then exit 42; fi')

    def stub(self, name, body):
        path = self.bin / name
        path.write_text(f"#!/bin/sh\n{body}\n")
        path.chmod(0o700)

    def run_setup(self, apt_failure=""):
        return subprocess.run(
            ["/bin/sh", "-e", "-c", self.script],
            cwd=self.workspace,
            env={"PATH": os.fspath(self.bin), "GITHUB_WORKSPACE": os.fspath(self.workspace),
                 "TRACE": os.fspath(self.trace), "APT_FAILURE": apt_failure},
            capture_output=True, text=True, check=False,
        )

    def assert_commands(self, *apt_commands):
        self.assertEqual(self.trace.read_text().splitlines(), [
            f"git <config> <--global> <--add> <safe.directory> <{self.workspace}>",
            *apt_commands,
        ])

    def test_healthy_python_skips_apt_in_isolated_mode(self):
        (self.bin / "python3").symlink_to(sys.executable)
        shadow = self.workspace / "argparse.py"
        shadow.write_text('raise RuntimeError("checkout module shadowed the standard library")\n')
        result = self.run_setup()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_commands()
        self.assertFalse((self.workspace / "__pycache__").exists())

    def test_missing_python_uses_apt(self):
        result = self.run_setup()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_commands(*APT_COMMANDS)

    def test_unusable_python_uses_apt(self):
        self.stub("python3", "exit 1")
        result = self.run_setup()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_commands(*APT_COMMANDS)

    def test_apt_failure_stops_setup(self):
        for apt_failure, expected_commands in (("update", APT_COMMANDS[:1]), ("install", APT_COMMANDS)):
            with self.subTest(apt_failure=apt_failure):
                self.trace.write_text("")
                result = self.run_setup(apt_failure=apt_failure)
                self.assertEqual(result.returncode, 42, result.stderr)
                self.assert_commands(*expected_commands)
