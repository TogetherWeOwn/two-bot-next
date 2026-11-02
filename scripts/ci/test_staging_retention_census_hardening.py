"""Review-round pins for the retention census; never touches staging.

Companion to the immutable `test_staging_retention_census.py` acceptance
handoff (which this file must not duplicate or edit): it pins two review
findings. Every numeric position fails closed as ReadFailed on huge input
(past Python's 4300-digit int() string limit), and text-mode failures keep
the fixed refusal/read cause ahead of the structured line while JSON stays
reason-free. No failure text may quote psql output or the login.
"""

import io
import json
import subprocess
import sys
import unittest
from contextlib import redirect_stdout
from datetime import datetime, timezone
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

import staging_retention_census as census


HOST = "staging-direct.neon.example"
DATABASE = "two_bot"
PASSWORD = "s3cret-fixture-only"
GOOD_URL = f"postgres://two_bot_migrator_ro_plan:{PASSWORD}@{HOST}:5432/{DATABASE}"
IDENTITY_OUT = "two_bot_migrator_ro,two_bot,f\n"
HUGE = "9" * 5000


def good_env(**overrides):
    env = {census.URL_ENV: GOOD_URL,
           census.HOST_ENV: HOST,
           census.DATABASE_ENV: DATABASE,
           census.BRANCH_ENV: ""}
    env.update(overrides)
    return env


def run_main(argv, env, fake=None):
    stdout, stderr = io.StringIO(), io.StringIO()
    runner = mock.patch("subprocess.run", fake) if fake else mock.patch(
        "subprocess.run", side_effect=AssertionError("no psql call expected"))
    with runner, redirect_stdout(stdout):
        with mock.patch.object(sys, "stderr", stderr):
            status = census.main(
                argv, env=env, clock=lambda: datetime.now(timezone.utc),
                psql="psql", sleep_fn=lambda seconds: None)
    return status, stdout.getvalue(), stderr.getvalue()


class HugeNumericTests(unittest.TestCase):
    def test_huge_numerics_fail_closed_as_read_failed(self):
        digest = "9f" * 32
        minute = "2026-10-20 20:00:00+00"
        cases = {
            "migration version": lambda: census.parse_migrations(
                f"{HUGE},2026-10-19 23:19:00+00,t\n"),
            "cohort count (digest)": lambda: census.parse_cohort(
                f"{HUGE},{digest},{minute},2026-10-20 20:10:00+00\n", True),
            "cohort count (no digest)": lambda: census.parse_cohort(
                f"{HUGE},{minute},2026-10-20 20:10:00+00\n", False),
            "family counts (eligible)": lambda: census.parse_counts(
                f"PRESENCE_UPDATE,{HUGE},900,907\n"),
            "family counts (fresh)": lambda: census.parse_counts(
                f"PRESENCE_UPDATE,1,{HUGE},907\n"),
            "family counts (total)": lambda: census.parse_counts(
                f"PRESENCE_UPDATE,1,900,{HUGE}\n"),
            "anchor": lambda: census.parse_anchor(f"{HUGE}\n"),
        }
        for label, call in cases.items():
            with self.subTest(position=label):
                with self.assertRaises(census.ReadFailed):
                    call()

    def test_huge_migration_version_fails_run_census_as_read_failed(self):
        huge_migrations = f"{HUGE},2026-10-19 23:19:00+00,t\n"

        def fake(command, input=None, **kwargs):
            if "current_user" in input:
                return subprocess.CompletedProcess(command, 0, IDENTITY_OUT, "")
            if "digest('retention-census-probe'" in input:
                return subprocess.CompletedProcess(command, 0, "ab12\n", "")
            if "_sqlx_migrations" in input:
                return subprocess.CompletedProcess(
                    command, 0, huge_migrations, "")
            raise AssertionError("census must stop at the malformed ledger row")

        with mock.patch("subprocess.run", fake):
            with self.assertRaises(census.ReadFailed):
                census.run_census(good_env(), "psql",
                                  lambda: datetime.now(timezone.utc),
                                  lambda seconds: None)


class TextReasonTests(unittest.TestCase):
    def test_refusal_keeps_fixed_fence_in_text_stderr(self):
        pooler = "staging-pooler.neon.example"
        env = good_env(**{census.HOST_ENV: pooler})
        status, stdout, stderr = run_main([], env)
        self.assertEqual(status, 2)
        self.assertEqual(stdout, "")
        self.assertIn("refusing: the pinned staging host is a pooler endpoint",
                      stderr)
        self.assertIn("refused: retryable=false", stderr)
        self.assertNotIn(PASSWORD, stderr)

    def test_read_failure_keeps_fixed_cause_in_text_stderr(self):
        sentinel = "sentinel-auth-stderr-marker"

        def auth_fails(command, input=None, **kwargs):
            return subprocess.CompletedProcess(
                command, 1, "",
                f'psql: error: password authentication failed; {sentinel}\n')

        status, stdout, stderr = run_main([], good_env(), fake=auth_fails)
        self.assertEqual(status, 1)
        self.assertEqual(stdout, "")
        self.assertIn("authentication failed (psql exit 1)", stderr)
        self.assertIn("read_failed: retryable=true", stderr)
        self.assertNotIn(sentinel, stderr)
        self.assertNotIn(PASSWORD, stderr)

    def test_json_failures_carry_no_reason(self):
        status, stdout, stderr = run_main(
            ["--json"], good_env(**{census.URL_ENV: ""}))
        self.assertEqual(status, 2)
        document = json.loads(stdout)
        self.assertEqual(document["status"], "refused")
        self.assertEqual(set(document["error"]),
                         {"code", "retryable", "next_action"})
        self.assertNotIn(PASSWORD, stdout)


if __name__ == "__main__":
    unittest.main()
