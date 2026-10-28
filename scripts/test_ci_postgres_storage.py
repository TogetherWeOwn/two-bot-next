"""Offline regressions for the disposable check service's storage contract."""

import contextlib
import io
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import textwrap
import unittest
from unittest import mock


WORKFLOW = Path(__file__).resolve().parents[1] / ".github/workflows/check.yml"
WORKFLOW_TEXT = WORKFLOW.read_text()
NEXTEST_WORKFLOW = (
    Path(__file__).resolve().parents[1] / ".github/workflows/reusable-rust-nextest.yml"
)
NEXTEST_WORKFLOW_TEXT = NEXTEST_WORKFLOW.read_text()
# The four Rust test lanes (rust-tests, coverage-shard, ignored-db-stores,
# ignored-db-runtime) each run their own copy of this service; the first one
# in the file is the reference and the others must match it exactly.
SERVICES = [part.split("    steps:\n", 1)[0] for part in WORKFLOW_TEXT.split("      agent-testdb:\n")[1:]]
SERVICE = SERVICES[0]
TOKENS = shlex.split(SERVICE.split("        options: >-\n", 1)[1])
OPTIONS = dict(zip(TOKENS[::2], TOKENS[1::2]))
VERIFY_TAILS = WORKFLOW_TEXT.split("      - name: Verify disposable Postgres defaults\n")[1:]
PREPARE_TAILS = WORKFLOW_TEXT.split("      - name: Prepare isolated test databases\n")[1:]
SCRIPTS = [
    textwrap.dedent(tail.split("python3 - <<'PY'\n", 1)[1].split("\n          PY", 1)[0])
    for tail in VERIFY_TAILS
]
PREPARES = [tail.split("\n      - name: ", 1)[0] for tail in PREPARE_TAILS]
NEXTEST_VERIFY_TAILS = NEXTEST_WORKFLOW_TEXT.split("      - name: Verify disposable Postgres defaults\n")[1:]
NEXTEST_SCRIPT = textwrap.dedent(
    NEXTEST_VERIFY_TAILS[0].split("python3 - <<'PY'\n", 1)[1].split("\n          PY", 1)[0]
)
POSTGRES_LANES = ("rust-tests", "coverage-shard", "ignored-db-stores", "ignored-db-runtime")
# Coverage legs need earlier checkpoints on the same 1 GiB tmpfs. Only
# its WAL targets differ; all five probes must enforce the same safety checks.
WAL_BUDGETS = {
    "rust-tests": ("128MB", "32MB"),
    "coverage-shard": ("128MB", "32MB"),
    "ignored-db-stores": ("512MB", "64MB"),
    "ignored-db-runtime": ("512MB", "64MB"),
    "nextest": ("512MB", "64MB"),
}
PROBES = list(zip(POSTGRES_LANES, SCRIPTS)) + [("nextest", NEXTEST_SCRIPT)]


def job_text(job_key):
    """Raw text of one top-level job block in check.yml."""
    lines = WORKFLOW_TEXT.splitlines(keepends=True)
    start = next(
        i for i, line in enumerate(lines)
        if re.match(rf"^  {re.escape(job_key)}:\s*$", line)
    )
    taken = []
    for line in lines[start + 1:]:
        if line.strip() and re.match(r"^  \S", line):
            break
        taken.append(line)
    return "".join(taken)
EXPECTED = {
    "data_directory": "/var/lib/postgresql/18/docker",
    "fsync": "on",
    "full_page_writes": "on",
    "synchronous_commit": "on",
}


def expected_settings(lane):
    max_wal, min_wal = WAL_BUDGETS[lane]
    return EXPECTED | {"max_wal_size": max_wal, "min_wal_size": min_wal}


class PostgresStorageTests(unittest.TestCase):
    def test_only_the_rust_test_lanes_get_bounded_pg18_storage(self):
        self.assertEqual(len(TOKENS), len(OPTIONS) * 2)
        self.assertIn("image: postgres:18.6@sha256:", SERVICE)
        self.assertEqual(OPTIONS["--tmpfs"], "/var/lib/postgresql:rw,size=1073741824")
        self.assertEqual(OPTIONS["--memory"], "2147483648")
        self.assertEqual(OPTIONS["--memory-swap"], OPTIONS["--memory"])
        # One bounded service per lane, byte-identical: the lanes must keep
        # sharing the config the single `check` job had.
        self.assertEqual(WORKFLOW_TEXT.count("--tmpfs /var/lib/postgresql:"), 4)
        self.assertEqual(WORKFLOW_TEXT.count("image: postgres:18.6@sha256:5a5a84b19854a9ffaa54082c166ff4ec27473a361e496e5ea167f298f2da9722"), 4)
        bounded = [service for service in SERVICES if "--tmpfs" in service]
        self.assertEqual(len(bounded), 4)
        self.assertEqual(len(set(bounded)), 1, "the lanes' Postgres services drifted apart")
        self.assertNotIn("/var/lib/postgresql/data", SERVICE)
        self.assertNotIn("PGDATA:", SERVICE)
        self.assertNotIn("POSTGRES_INITDB_ARGS:", SERVICE)
        self.assertEqual(OPTIONS["--health-interval"], "5s")
        self.assertEqual(OPTIONS["--health-timeout"], "5s")
        self.assertEqual(OPTIONS["--health-retries"], "12")

    def test_health_requires_actual_pgdata_tmpfs_and_a_ready_server(self):
        command = OPTIONS["--health-cmd"]
        self.assertIn("stat -f -c %T $PGDATA", command)
        self.assertIn("pg_isready -U agent_test -d agent_test", command)
        for filesystem, ready, expected in [("tmpfs", 0, 0), ("ext4", 0, 1), ("tmpfs", 1, 1)]:
            with self.subTest(filesystem=filesystem, ready=ready):
                stubs = (
                    f"stat() {{ printf '%s\\n' {filesystem}; }}; "
                    f"pg_isready() {{ return {ready}; }}; "
                    "PGDATA=/var/lib/postgresql/18/docker; "
                )
                result = subprocess.run(["sh", "-c", stubs + command], check=False)
                self.assertEqual(result.returncode, expected)

    def test_all_four_lanes_bound_wal_and_verify_it(self):
        # Run 39209332791: only rust-tests bounded WAL while the ignored lanes
        # ran the PG18 default (max_wal_size 1GB) on the same 1 GiB tmpfs, and
        # the stores lane PANICed with ENOSPC mid-suite. Every lane that mounts
        # the bounded service must carry the budget and verify it. Scope the
        # checks per job: a global count lets one lane's two prepares satisfy
        # another lane's missing one.
        self.assertEqual(len(PREPARE_TAILS), 4)
        self.assertEqual(len(VERIFY_TAILS), 4)
        scripts = []
        for lane in POSTGRES_LANES:
            with self.subTest(lane=lane):
                job = job_text(lane)
                self.assertIn("agent-testdb:", job)
                self.assertIn("--tmpfs /var/lib/postgresql:", job)
                self.assertEqual(
                    job.count("      - name: Prepare isolated test databases\n"), 1,
                    f"{lane} must carry exactly one WAL prepare step",
                )
                self.assertEqual(
                    job.count("      - name: Verify disposable Postgres defaults\n"), 1,
                    f"{lane} must carry exactly one WAL verify step",
                )
                prepare = job.split(
                    "      - name: Prepare isolated test databases\n", 1,
                )[1].split("\n      - name: ", 1)[0]
                max_wal, min_wal = WAL_BUDGETS[lane]
                self.assertIn(f"ALTER SYSTEM SET max_wal_size = '{max_wal}'", prepare)
                self.assertIn(f"ALTER SYSTEM SET min_wal_size = '{min_wal}'", prepare)
                self.assertIn("pg_reload_conf()", prepare)
                self.assertLess(
                    prepare.index("ALTER SYSTEM SET max_wal_size"),
                    prepare.index("pg_reload_conf()"),
                )
                self.assertLess(
                    prepare.index("ALTER SYSTEM SET min_wal_size"),
                    prepare.index("pg_reload_conf()"),
                )
                verify_tail = job.split(
                    "      - name: Verify disposable Postgres defaults\n", 1,
                )[1]
                script = textwrap.dedent(
                    verify_tail.split("python3 - <<'PY'\n", 1)[1].split("\n          PY", 1)[0]
                )
                self.assertIn(f"'max_wal_size': '{max_wal}'", script)
                self.assertIn(f"'min_wal_size': '{min_wal}'", script)
                # Normalize only the separately pinned WAL literals. Any change
                # to connection, durability or rejection logic must still fail.
                scripts.append(
                    script.replace(f"'max_wal_size': '{max_wal}'", "'max_wal_size': '512MB'")
                    .replace(f"'min_wal_size': '{min_wal}'", "'min_wal_size': '64MB'")
                )
        self.assertEqual(len(set(scripts)), 1, "the lanes' verify probes drifted apart")
        # Nextest retains the larger budget. Compare its complete safety logic
        # too, and execute every copy in the behavioral tests below.
        self.assertEqual(len(NEXTEST_VERIFY_TAILS), 1)
        nextest_max, nextest_min = WAL_BUDGETS["nextest"]
        normalized_nextest = (
            NEXTEST_SCRIPT.replace(
                f"'max_wal_size': '{nextest_max}'", "'max_wal_size': '512MB'"
            ).replace(f"'min_wal_size': '{nextest_min}'", "'min_wal_size': '64MB'")
        )
        self.assertEqual(
            normalized_nextest, scripts[0],
            "the nextest verify probe drifted from the executed reference",
        )

    def test_probe_preserves_durability_and_uses_only_explicit_test_connection(self):
        for lane, script in PROBES:
            expected = expected_settings(lane)
            output = io.StringIO()
            with (
                self.subTest(lane=lane),
                mock.patch.dict(os.environ, {}, clear=True),
                mock.patch("subprocess.check_output", return_value=json.dumps(expected)) as probe,
                contextlib.redirect_stdout(output),
            ):
                exec(compile(script, str(WORKFLOW), "exec"), {})
                args, kwargs = probe.call_args
                self.assertEqual(args[0][:8], [
                    "psql", "postgres://agent_test:@agent-testdb:5432/two_bot_test_ci",
                    "-X", "-w", "-At", "-v", "ON_ERROR_STOP=1", "-c",
                ])
                for key in expected:
                    self.assertIn(f"current_setting('{key}')", args[0][8])
                self.assertEqual(kwargs["env"], {
                    "PGPASSFILE": "/dev/null", "PGSSLMODE": "disable",
                    "PGOPTIONS": "-c statement_timeout=5000",
                })
                self.assertIn("Disposable Postgres defaults:", output.getvalue())

    def test_probe_rejects_changed_path_or_disabled_durability(self):
        for lane, script in PROBES:
            expected = expected_settings(lane)
            for key in expected:
                changed = expected | {key: "changed"}
                with (
                    self.subTest(lane=lane, key=key),
                    mock.patch.dict(os.environ, {}, clear=True),
                    mock.patch("subprocess.check_output", return_value=json.dumps(changed)),
                    self.assertRaisesRegex(SystemExit, "data path or durability defaults changed"),
                ):
                    exec(compile(script, str(WORKFLOW), "exec"), {})

    def test_probe_refuses_ambient_redirection_before_connecting(self):
        for lane, script in PROBES:
            with (
                self.subTest(lane=lane),
                mock.patch.dict(os.environ, {"PGHOSTADDR": "fixture-redirection"}, clear=True),
                mock.patch("subprocess.check_output") as probe,
                self.assertRaisesRegex(SystemExit, "unset ambient libpq"),
            ):
                exec(compile(script, str(WORKFLOW), "exec"), {})
            probe.assert_not_called()

    def test_failed_probe_cannot_claim_defaults_verified(self):
        for lane, script in PROBES:
            with (
                self.subTest(lane=lane),
                mock.patch.dict(os.environ, {}, clear=True),
                mock.patch("subprocess.check_output", side_effect=subprocess.CalledProcessError(1, "psql")),
                self.assertRaises(subprocess.CalledProcessError),
            ):
                exec(compile(script, str(WORKFLOW), "exec"), {})


if __name__ == "__main__":
    unittest.main()
