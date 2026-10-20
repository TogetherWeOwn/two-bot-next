"""Execute the current Rust welcome runtime against socket-free boundaries.

Run from the repository root:
  PYTHONDONTWRITEBYTECODE=1 python3 tests/onboarding/test_welcome_path_acceptance.py -v

The example is deliberately built without cfg(test), avoiding the runtime's
existing live-SQL/TCP test helpers. The default build uses the bounded controller
pool. Hosted CI must first build the example from the same checkout, then set
WELCOME_PATH_ACCEPTANCE_EXECUTABLE to its executable path to run without a build.
An explicit missing/nonexecutable path is an ERROR, never a skip or build fallback.
A refused/failed build is an ERROR, never behavioral RED. No cargo run, private
Cargo target, database URL, credential, listener, or network fixture is used.
This suite is a test-author branch handoff; it must not merge while RED. The
implementation PR must route this command in CI as well as passing it locally.
"""

import json
import os
from pathlib import Path
import subprocess
import sys
import unittest

ROOT = Path(__file__).resolve().parents[2]
CASES = (
    "anchor_traps",
    "repeated_window",
    "expiry",
    "invalidation",
    "record_retry_legacy",
    "record_retry_anchor",
    "failed_send",
    "failed_send_anchor",
    "missing_identity",
    "duplicate_control",
    "live_channel_denial",
    "unavailable_identity",
    "roles_invalidation",
    "channel_invalidation",
    "guild_create_invalidation",
    "bot_member_invalidation",
    "roles_invalidation_revoked",
    "channel_invalidation_revoked",
    "roles_invalidation_exhausted",
    "channel_invalidation_exhausted",
)


class WelcomePathAcceptance(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if "WELCOME_PATH_ACCEPTANCE_EXECUTABLE" in os.environ:
            executable = Path(os.environ["WELCOME_PATH_ACCEPTANCE_EXECUTABLE"]).resolve()
            if not executable.is_file() or not os.access(executable, os.X_OK):
                raise RuntimeError(
                    "INFRASTRUCTURE ERROR (not behavioral RED): "
                    "WELCOME_PATH_ACCEPTANCE_EXECUTABLE must name an executable file"
                )
            cls.executable = str(executable)
            print("PREBUILT: executing same-checkout acceptance cases", flush=True)
            return
        command = [
            sys.executable, "scripts/cargo_cache.py", "run", "--", "build",
            "-p", "two-bot", "--example", "welcome_path_acceptance",
            "--locked", "--message-format=json",
        ]
        print("BUILD: " + " ".join(command), flush=True)
        build = subprocess.run(
            command, cwd=ROOT, capture_output=True, text=True, timeout=600,
            check=False,
        )
        executable = None
        messages = []
        for line in build.stdout.splitlines():
            try:
                record = json.loads(line)
            except json.JSONDecodeError:
                messages.append(line)
                continue
            if record.get("reason") == "compiler-message":
                messages.append(record["message"].get("rendered", ""))
            if (record.get("reason") == "compiler-artifact"
                    and record.get("target", {}).get("name") == "welcome_path_acceptance"):
                executable = record.get("executable") or executable
        if build.returncode != 0 or not executable:
            raise RuntimeError(
                "INFRASTRUCTURE/COMPILE ERROR (not behavioral RED), exit "
                + str(build.returncode) + "\n" + build.stderr + "\n" + "\n".join(messages)
            )
        if messages:
            print("\n".join(messages), flush=True)
        cls.executable = executable
        print("BUILD: success; executing current-source acceptance cases", flush=True)


def case_test(case):
    def run(self):
        result = subprocess.run(
            [self.executable, case], cwd=ROOT, capture_output=True,
            text=True, timeout=20, check=False,
        )
        output = result.stdout + result.stderr
        self.assertEqual(result.returncode, 0, f"{case}: exit {result.returncode}\n{output}")
        self.assertIn(f"PASS {case}", result.stdout)
    return run


for name in CASES:
    setattr(WelcomePathAcceptance, "test_" + name, case_test(name))

if __name__ == "__main__":
    unittest.main()
