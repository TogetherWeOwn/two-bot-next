"""Offline runner-contract tests; fixture executables are not Rust acceptance evidence."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

RUNNER = Path(__file__).with_name("test_welcome_path_acceptance.py")
ENV_KEY = "WELCOME_PATH_ACCEPTANCE_EXECUTABLE"
SCENARIOS = {
    "anchor_traps", "repeated_window", "expiry", "invalidation",
    "record_retry_legacy", "record_retry_anchor", "failed_send",
    "failed_send_anchor", "missing_identity", "duplicate_control",
    "live_channel_denial", "unavailable_identity",
    "roles_invalidation", "channel_invalidation",
    "guild_create_invalidation", "bot_member_invalidation",
    "roles_invalidation_revoked", "channel_invalidation_revoked",
    "roles_invalidation_exhausted", "channel_invalidation_exhausted",
}


class WelcomePathRunnerContract(unittest.TestCase):
    def setUp(self):
        scratch = os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR") or os.environ.get("PAPERCLIP_SCRATCH_DIR")
        temporary = tempfile.TemporaryDirectory(prefix="welcome-runner-", dir=scratch)
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.runner = self.root / "tests/onboarding/test_welcome_path_acceptance.py"
        self.runner.parent.mkdir(parents=True)
        shutil.copyfile(RUNNER, self.runner)
        self.executable = self.root / "prebuilt example"
        self.executable.write_text(
            f"#!{sys.executable}\n"
            "import os, pathlib, sys\n"
            "with pathlib.Path(os.environ['FIXTURE_RECORD']).open('a') as record:\n"
            "    record.write(sys.argv[1] + '\\n')\n"
            "if os.environ.get('FIXTURE_MODE') != 'silent':\n"
            "    print('PASS ' + sys.argv[1])\n"
            "sys.exit(7 if os.environ.get('FIXTURE_MODE') == 'red' else 0)\n"
        )
        self.executable.chmod(0o700)
        wrapper = self.root / "scripts/cargo_cache.py"
        wrapper.parent.mkdir()
        wrapper.write_text(
            "import json, os, pathlib, sys\n"
            "pathlib.Path(os.environ['FIXTURE_CARGO_RECORD']).write_text(json.dumps(sys.argv[1:]))\n"
            "if os.environ.get('FIXTURE_BUILD_FAIL'):\n"
            "    sys.exit(83)\n"
            "print(json.dumps({'reason': 'compiler-artifact',\n"
            "    'target': {'name': 'welcome_path_acceptance'},\n"
            "    'executable': os.environ['FIXTURE_EXECUTABLE']}))\n"
        )
        self.record = self.root / "cases.txt"
        self.cargo_record = self.root / "cargo.json"
        self.env = dict(os.environ)
        for key in (ENV_KEY, "FIXTURE_MODE", "FIXTURE_BUILD_FAIL"):
            self.env.pop(key, None)
        self.env.update(
            PYTHONDONTWRITEBYTECODE="1",
            FIXTURE_EXECUTABLE=str(self.executable),
            FIXTURE_RECORD=str(self.record),
            FIXTURE_CARGO_RECORD=str(self.cargo_record),
        )

    def run_runner(self, override=None, **environment):
        env = self.env | environment
        if override is not None:
            env[ENV_KEY] = str(override)
        return subprocess.run(
            [sys.executable, str(self.runner), "-v"], cwd=self.root,
            env=env, capture_output=True, text=True, timeout=30, check=False,
        )

    def test_explicit_prebuilt_runs_every_scenario_without_building(self):
        result = self.run_runner(self.executable)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        cases = self.record.read_text().splitlines()
        self.assertEqual(set(cases), SCENARIOS)
        self.assertEqual(len(cases), 20)
        self.assertFalse(self.cargo_record.exists(), "prebuilt mode invoked the bounded builder")

    def test_invalid_explicit_paths_error_without_fallback_or_skip(self):
        nonexecutable = self.root / "not executable"
        nonexecutable.write_text("not an executable\n")
        nonexecutable.chmod(0o600)
        for path in (self.root / "missing", nonexecutable, self.root, ""):
            with self.subTest(path=str(path)):
                result = self.run_runner(path)
                self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertIn("ERROR", result.stderr)
                self.assertIn(ENV_KEY, result.stderr)
                self.assertNotIn("skipped", result.stderr.lower())
                self.assertFalse(self.cargo_record.exists(), "invalid override fell back to a build")
                self.assertFalse(self.record.exists(), "invalid override ran a scenario")

    def test_nonzero_case_exit_is_still_behavioral_red(self):
        result = self.run_runner(self.executable, FIXTURE_MODE="red")
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("FAILED (failures=20)", result.stderr)
        self.assertIn("exit 7", result.stderr)
        self.assertFalse(self.cargo_record.exists())

    def test_missing_pass_marker_is_still_behavioral_red(self):
        result = self.run_runner(self.executable, FIXTURE_MODE="silent")
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("FAILED (failures=20)", result.stderr)
        self.assertIn("not found", result.stderr)
        self.assertFalse(self.cargo_record.exists())

    def test_unset_override_keeps_bounded_build_and_artifact_selection(self):
        result = self.run_runner()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(json.loads(self.cargo_record.read_text()), [
            "run", "--", "build", "-p", "two-bot", "--example",
            "welcome_path_acceptance", "--locked", "--message-format=json",
        ])
        self.assertEqual(set(self.record.read_text().splitlines()), SCENARIOS)

    def test_default_build_refusal_is_error_not_behavioral_red_or_skip(self):
        result = self.run_runner(FIXTURE_BUILD_FAIL="1")
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("INFRASTRUCTURE/COMPILE ERROR (not behavioral RED), exit 83", result.stderr)
        self.assertIn("FAILED (errors=1)", result.stderr)
        self.assertNotIn("skipped", result.stderr.lower())
        self.assertFalse(self.record.exists())


if __name__ == "__main__":
    unittest.main()
