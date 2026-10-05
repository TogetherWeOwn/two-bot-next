"""Offline regressions for the disposable check service's storage contract."""

import contextlib
import io
import json
import os
from pathlib import Path
import shlex
import subprocess
import textwrap
import unittest
from unittest import mock


WORKFLOW = Path(__file__).resolve().parents[1] / ".github/workflows/check.yml"
WORKFLOW_TEXT = WORKFLOW.read_text()
# The three Rust test lanes (rust-tests, ignored-db-stores, ignored-db-runtime)
# each run their own copy of this service; the first one in the file is the
# reference and the others must match it exactly.
SERVICES = [part.split("    steps:\n", 1)[0] for part in WORKFLOW_TEXT.split("      agent-testdb:\n")[1:]]
SERVICE = SERVICES[0]
TOKENS = shlex.split(SERVICE.split("        options: >-\n", 1)[1])
OPTIONS = dict(zip(TOKENS[::2], TOKENS[1::2]))
STEP = WORKFLOW_TEXT.split("      - name: Verify disposable Postgres defaults\n", 1)[1].split(
    "      # Preserve cargo test's default", 1
)[0]
SCRIPT = textwrap.dedent(STEP.split("python3 - <<'PY'\n", 1)[1].rsplit("          PY", 1)[0])
EXPECTED = {
    "data_directory": "/var/lib/postgresql/18/docker",
    "fsync": "on",
    "full_page_writes": "on",
    "synchronous_commit": "on",
}


class PostgresStorageTests(unittest.TestCase):
    def test_only_the_rust_test_lanes_get_bounded_pg18_storage(self):
        self.assertEqual(len(TOKENS), len(OPTIONS) * 2)
        self.assertIn("image: postgres:18.6@sha256:", SERVICE)
        self.assertEqual(OPTIONS["--tmpfs"], "/var/lib/postgresql:rw,size=1073741824")
        self.assertEqual(OPTIONS["--memory"], "2147483648")
        self.assertEqual(OPTIONS["--memory-swap"], OPTIONS["--memory"])
        # One bounded service per lane, byte-identical: the lanes must keep
        # sharing the config the single `check` job had.
        self.assertEqual(WORKFLOW_TEXT.count("--tmpfs /var/lib/postgresql:"), 3)
        self.assertEqual(WORKFLOW_TEXT.count("image: postgres:18.6@sha256:5a5a84b19854a9ffaa54082c166ff4ec27473a361e496e5ea167f298f2da9722"), 3)
        bounded = [service for service in SERVICES if "--tmpfs" in service]
        self.assertEqual(len(bounded), 3)
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

    def test_probe_preserves_durability_and_uses_only_explicit_test_connection(self):
        output = io.StringIO()
        with (
            mock.patch.dict(os.environ, {}, clear=True),
            mock.patch("subprocess.check_output", return_value=json.dumps(EXPECTED)) as probe,
            contextlib.redirect_stdout(output),
        ):
            exec(compile(SCRIPT, str(WORKFLOW), "exec"), {})
        args, kwargs = probe.call_args
        self.assertEqual(args[0][:8], [
            "psql", "postgres://agent_test:@agent-testdb:5432/two_bot_test_ci",
            "-X", "-w", "-At", "-v", "ON_ERROR_STOP=1", "-c",
        ])
        for key in EXPECTED:
            self.assertIn(f"current_setting('{key}')", args[0][8])
        self.assertEqual(kwargs["env"], {
            "PGPASSFILE": "/dev/null", "PGSSLMODE": "disable",
            "PGOPTIONS": "-c statement_timeout=5000",
        })
        self.assertIn("Disposable Postgres defaults:", output.getvalue())

    def test_probe_rejects_changed_path_or_disabled_durability(self):
        for key in EXPECTED:
            changed = EXPECTED | {key: "changed"}
            with (
                self.subTest(key=key),
                mock.patch.dict(os.environ, {}, clear=True),
                mock.patch("subprocess.check_output", return_value=json.dumps(changed)),
                self.assertRaisesRegex(SystemExit, "data path or durability defaults changed"),
            ):
                exec(compile(SCRIPT, str(WORKFLOW), "exec"), {})

    def test_probe_refuses_ambient_redirection_before_connecting(self):
        with (
            mock.patch.dict(os.environ, {"PGHOSTADDR": "fixture-redirection"}, clear=True),
            mock.patch("subprocess.check_output") as probe,
            self.assertRaisesRegex(SystemExit, "unset ambient libpq"),
        ):
            exec(compile(SCRIPT, str(WORKFLOW), "exec"), {})
        probe.assert_not_called()

    def test_failed_probe_cannot_claim_defaults_verified(self):
        with (
            mock.patch.dict(os.environ, {}, clear=True),
            mock.patch("subprocess.check_output", side_effect=subprocess.CalledProcessError(1, "psql")),
            self.assertRaises(subprocess.CalledProcessError),
        ):
            exec(compile(SCRIPT, str(WORKFLOW), "exec"), {})


if __name__ == "__main__":
    unittest.main()
