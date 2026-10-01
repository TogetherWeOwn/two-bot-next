"""Offline checks for CI-only observation; no PostgreSQL or Cargo calls."""

import importlib.util
import io
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("ci_db_waits", Path(__file__).with_name("ci-db-waits.py"))
waits = importlib.util.module_from_spec(spec)
spec.loader.exec_module(waits)


class WaitObserverTests(unittest.TestCase):
    def test_refuses_controller_and_non_check_jobs(self):
        for environment in ({}, {"GITHUB_ACTIONS": "true"}, {"TWO_LFG_TESTDB_CI": "1"}):
            with self.subTest(environment=environment), patch.dict(os.environ, environment, clear=True):
                with self.assertRaisesRegex(RuntimeError, "ephemeral check job"):
                    waits.service_environment()

    def test_fixed_test_service_never_inherits_credentials(self):
        with patch.dict(os.environ, {
            "GITHUB_ACTIONS": "true", "TWO_LFG_TESTDB_CI": "1", "PATH": "/bin",
            "PGHOST": "production.invalid", "PGUSER": "other", "PGPASSWORD": "fixture-secret",
            "PGSERVICE": "production", "DATABASE_URL": "postgres://production.invalid",
        }, clear=True):
            environment = waits.service_environment()
        self.assertEqual(environment["PGHOST"], "agent-testdb")
        self.assertEqual(environment["PGUSER"], "agent_test")
        self.assertEqual(environment["PGDATABASE"], "two_bot_test_ci")
        self.assertEqual(environment["PGPASSWORD"], "")
        self.assertEqual(environment["PGPASSFILE"], "/dev/null")
        self.assertNotIn("PGSERVICE", environment)
        self.assertNotIn("DATABASE_URL", environment)
        self.assertNotIn("fixture-secret", str(environment))
        self.assertIn("default_transaction_read_only=on", environment["PGOPTIONS"])

    def test_sample_is_bounded_and_records_only_sanitized_projection(self):
        record = {"at": "fixture", "ddl": [{"operation": "drop", "wait_event": "CheckpointDone"}]}
        log = io.StringIO()
        with patch.object(waits.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, json.dumps(record))) as run:
            waits.sample({"PGHOST": "agent-testdb"}, log)
        self.assertEqual(json.loads(log.getvalue()), record)
        self.assertEqual(run.call_args.kwargs["timeout"], 3)
        self.assertIn("--no-psqlrc", run.call_args.args[0])
        self.assertIn("--no-password", run.call_args.args[0])
        self.assertNotIn("'query',", waits.QUERY)
        self.assertIn("pg_blocking_pids(pid)", waits.QUERY)
        self.assertIn("pg_stat_checkpointer", waits.QUERY)

    def test_checkpointer_projection_exposes_only_waits_and_io_counters(self):
        activity = waits.QUERY.split("'checkpointer_activity',", 1)[1].split("'checkpointer_io',", 1)[0]
        self.assertIn("SELECT pid, state, wait_event_type AS wait_type, wait_event", activity)
        self.assertIn("FROM pg_stat_activity WHERE backend_type = 'checkpointer'", activity)
        io = waits.QUERY.split("'checkpointer_io',", 1)[1].split("'observer_settings',", 1)[0]
        self.assertIn("SELECT object, context, writes, write_time, writebacks, writeback_time,", io)
        self.assertIn("fsyncs, fsync_time, stats_reset", io)
        self.assertIn("FROM pg_stat_io WHERE backend_type = 'checkpointer'", io)
        self.assertEqual(activity.count("SELECT"), 2)
        self.assertEqual(io.count("SELECT"), 2)
        for projection in (activity, io):
            self.assertNotIn("*", projection)
            self.assertNotRegex(projection, r"\b(query|usename|client_addr|datname)\b")
        self.assertIn("sync_time, buffers_written, stats_reset", waits.QUERY)

    def test_observer_settings_are_allowlisted_and_read_only(self):
        names = re.findall(r"current_setting\('([^']+)'\)", waits.QUERY)
        self.assertEqual(names, [
            "server_version_num", "track_io_timing", "track_wal_io_timing",
            "fsync", "full_page_writes", "synchronous_commit", "wal_sync_method",
            "checkpoint_timeout", "checkpoint_completion_target", "checkpoint_flush_after",
        ])
        self.assertNotIn("pg_settings", waits.QUERY)
        self.assertNotIn("set_config", waits.QUERY)
        self.assertNotIn("pg_stat_reset", waits.QUERY)
        self.assertNotRegex(waits.QUERY, r"(?im)^\s*(ALTER|SET|CHECKPOINT|UPDATE|DELETE|INSERT)\b")
        for name in names:
            self.assertIn(f"'{name}', current_setting('{name}')", waits.QUERY)

    def test_sample_preserves_disabled_timing_null_counters_and_empty_activity(self):
        for activity in ([], [{"pid": 74, "state": None, "wait_type": "IO", "wait_event": "DataFileSync"}]):
            record = {
                "at": "fixture", "ddl": [], "checkpointer_activity": activity,
                "checkpointer_io": [{"object": "relation", "context": "normal",
                                     "writes": 3136, "write_time": 0, "writebacks": None,
                                     "writeback_time": None, "fsyncs": 694, "fsync_time": 0,
                                     "stats_reset": "fixture-reset"}],
                "observer_settings": {"track_io_timing": "off", "track_wal_io_timing": "off",
                                      "fsync": "on", "full_page_writes": "on"},
            }
            with self.subTest(activity=activity):
                log = io.StringIO()
                with patch.object(waits.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, json.dumps(record))):
                    waits.sample({}, log)
                self.assertEqual(json.loads(log.getvalue()), record)

    def test_query_failure_does_not_expose_stderr_or_retry(self):
        with patch.object(waits.subprocess, "run", return_value=subprocess.CompletedProcess([], 2, "", "fixture-secret")) as run:
            with self.assertRaisesRegex(RuntimeError, "psql exit 2") as error:
                waits.sample({}, io.StringIO())
        self.assertNotIn("fixture-secret", str(error.exception))
        self.assertEqual(run.call_count, 1)

    def test_stderr_hints_are_fixed_labels_and_unknown_text_stays_unclassified(self):
        cases = [
            ('could not translate host name "fixture-secret" to address', "name-resolution"),
            ("connection failed: Connection refused", "connection-refused"),
            ("connection failed: timeout expired", "connect-timeout"),
            ('FATAL: password authentication failed for user "fixture-secret"', "authentication-rejected"),
            ("FATAL: no pg_hba.conf entry for host fixture-secret", "authentication-rejected"),
            ("FATAL: remaining connection slots are reserved", "connection-limit"),
            ("FATAL: sorry, too many clients already", "connection-limit"),
            ("server closed the connection unexpectedly", "connection-lost"),
            ("connection to server was lost", "connection-lost"),
            ("FATAL: terminating connection due to administrator command", "connection-lost"),
            ("FATAL: the database system is starting up", "server-unavailable"),
            ("FATAL: the database system is shutting down", "server-unavailable"),
            ("FATAL: the database system is in recovery mode", "server-unavailable"),
            ("ERROR: canceling statement due to statement timeout", "statement-timeout"),
            ("fixture-secret\nLINE 1: SELECT private_input", "unclassified"),
            (None, "unclassified"),
        ]
        for stderr, expected in cases:
            with self.subTest(stderr=stderr):
                self.assertEqual(waits.stderr_hint(stderr), expected)

    def test_psql_failure_receipt_is_timed_sanitized_and_not_a_server_sample(self):
        record = {"at": "fixture", "ddl": []}
        log = io.StringIO()
        failed = subprocess.CompletedProcess([], 2, "fixture-secret-output",
                                             "timeout expired; fixture-secret /private/path SELECT private_input")
        with patch.object(waits.subprocess, "run", side_effect=[subprocess.CompletedProcess([], 0, json.dumps(record)), failed]) as run, \
                patch.object(waits.time, "monotonic", side_effect=[1, 2, 2.125]), \
                patch.object(log, "flush", wraps=log.flush) as flush:
            waits.sample({}, log)
            with self.assertRaisesRegex(RuntimeError, "psql exit 2; connect-timeout") as error:
                waits.sample({}, log)
        receipts = [json.loads(line) for line in log.getvalue().splitlines()]
        self.assertEqual(receipts[0], record)
        self.assertEqual(set(receipts[1]), {"observer_error"})
        failure = receipts[1]["observer_error"]
        self.assertEqual(set(failure), {"client_at", "elapsed_ms", "psql_exit", "stderr_hint"})
        self.assertRegex(failure["client_at"], r"^\d{4}-\d{2}-\d{2}T.*\+00:00$")
        self.assertEqual(failure["elapsed_ms"], 125)
        self.assertEqual(failure["psql_exit"], 2)
        self.assertEqual(failure["stderr_hint"], "connect-timeout")
        for private in ("fixture-secret", "/private/path", "SELECT private_input"):
            self.assertNotIn(private, log.getvalue() + str(error.exception))
        self.assertEqual(run.call_count, 2)  # One successful sample, one failed attempt; no retry.
        self.assertTrue(all(call.kwargs["timeout"] == 3 for call in run.call_args_list))
        self.assertEqual(flush.call_count, 2)

    def test_process_failures_record_fixed_hints_without_exception_input(self):
        cases = [
            (subprocess.TimeoutExpired(["fixture-secret-command"], 3,
                                       output=b"fixture-secret-output", stderr=b"fixture-secret-error"),
             "client-process-timeout"),
            (OSError("fixture-secret-path"), "client-process-error"),
        ]
        for exception, expected in cases:
            with self.subTest(hint=expected):
                log = io.StringIO()
                with patch.object(waits.subprocess, "run", side_effect=exception) as run, \
                        patch.object(waits.time, "monotonic", side_effect=[1, 1.25]):
                    with self.assertRaises(RuntimeError) as error:
                        waits.sample({}, log)
                failure = json.loads(log.getvalue())["observer_error"]
                self.assertEqual(failure["stderr_hint"], expected)
                self.assertEqual(failure["elapsed_ms"], 250)
                self.assertIsNone(failure["psql_exit"])
                self.assertNotIn("fixture-secret", log.getvalue() + str(error.exception))
                self.assertEqual(run.call_count, 1)

    def test_real_sample_failure_preserves_success_and_failure(self):
        import threading

        for child_exit in (0, 101):
            with self.subTest(child_exit=child_exit):
                log = io.StringIO()
                calls = []
                sample_failed = threading.Event()

                def invoke(command, **kwargs):
                    calls.append(command)
                    if command[0] == "psql":
                        if len([c for c in calls if c[0] == "psql"]) == 1:
                            return subprocess.CompletedProcess(command, 0, '{"at":"fixture","ddl":[]}')
                        sample_failed.set()
                        return subprocess.CompletedProcess(command, 2, "", "timeout expired fixture-secret")
                    self.assertTrue(sample_failed.wait(2), "observer must attempt its second sample")
                    return subprocess.CompletedProcess(command, child_exit)

                with patch.object(waits, "service_environment", return_value={}), \
                        patch.object(waits.subprocess, "run", side_effect=invoke), \
                        patch("sys.stdout", new_callable=io.StringIO) as output:
                    self.assertEqual(waits.run(["cargo", "test"], log), child_exit)
                self.assertEqual(len([c for c in calls if c[0] == "psql"]), 2)
                self.assertEqual([c for c in calls if c[0] != "psql"], [["cargo", "test"]])
                receipts = [json.loads(line) for line in log.getvalue().splitlines()]
                self.assertEqual(len(receipts), 2)
                self.assertEqual(receipts[1]["observer_error"]["stderr_hint"], "connect-timeout")
                self.assertIn("evidence incomplete", output.getvalue())
                self.assertNotIn("fixture-secret", log.getvalue() + output.getvalue())

    def test_failed_initial_sample_writes_receipt_and_starts_child_once(self):
        for child_exit in (0, 101):
            with self.subTest(child_exit=child_exit):
                log = io.StringIO()
                with patch.object(waits, "service_environment", return_value={}), \
                        patch.object(waits.subprocess, "run", side_effect=[
                            subprocess.CompletedProcess([], 2, "", "fixture-secret"),
                            subprocess.CompletedProcess(["cargo", "test"], child_exit),
                        ]) as run, patch("sys.stdout", new_callable=io.StringIO) as output:
                    self.assertEqual(waits.run(["cargo", "test"], log), child_exit)
                self.assertEqual(run.call_count, 2)
                self.assertEqual(run.call_args_list[0].args[0][0], "psql")
                self.assertEqual(run.call_args_list[1].args[0], ["cargo", "test"])
                self.assertTrue(log.getvalue(), "failed sample must leave an inspectable receipt")
                self.assertEqual(json.loads(log.getvalue())["observer_error"]["stderr_hint"], "unclassified")
                self.assertIn("::warning::", output.getvalue())
                self.assertNotIn("fixture-secret", log.getvalue() + output.getvalue())

    def test_preserves_test_failure_and_stops_observer(self):
        commands = [
            "cargo test --workspace --lib --bins --features two-bot-core/db --locked",
            "cargo test --workspace --test '*' --features two-bot-core/db --locked",
            "cargo test -p two-bot-discord --features db --locked --test internal_member_store -- --ignored",
        ]
        for command in commands:
            argv = shlex.split(command)
            with self.subTest(command=command), \
                    patch.object(waits, "service_environment", return_value={}), \
                    patch.object(waits, "sample"), \
                    patch.object(waits.subprocess, "run", return_value=subprocess.CompletedProcess(argv, 101)) as run:
                self.assertEqual(waits.run(argv, io.StringIO()), 101)
            run.assert_called_once_with(argv)

    def test_sampling_and_log_errors_are_warn_only_without_exposing_input(self):
        for exception in (RuntimeError("fixture-secret"), ValueError("fixture-secret"),
                          OSError("fixture-secret"), subprocess.TimeoutExpired(["fixture-secret"], 3)):
            for child_exit in (0, 101):
                with self.subTest(exception=type(exception), child_exit=child_exit), \
                        patch.object(waits, "service_environment", return_value={}), \
                        patch.object(waits, "sample", side_effect=exception) as sample, \
                        patch.object(waits.subprocess, "run", return_value=subprocess.CompletedProcess([], child_exit)) as run, \
                        patch("sys.stdout", new_callable=io.StringIO) as output:
                    self.assertEqual(waits.run(["cargo", "test"], io.StringIO()), child_exit)
                sample.assert_called_once()
                run.assert_called_once_with(["cargo", "test"])
                self.assertIn("::warning::DB wait evidence incomplete", output.getvalue())
                self.assertNotIn("fixture-secret", output.getvalue())

    def test_failed_thread_start_preserves_child_status(self):
        for child_exit in (0, 101):
            with self.subTest(child_exit=child_exit), \
                    patch.object(waits, "service_environment", return_value={}), \
                    patch.object(waits, "sample"), \
                    patch.object(waits.threading.Thread, "start", side_effect=RuntimeError("fixture-secret")), \
                    patch.object(waits.subprocess, "run", return_value=subprocess.CompletedProcess([], child_exit)) as run, \
                    patch("sys.stdout", new_callable=io.StringIO) as output:
                self.assertEqual(waits.run(["cargo", "test"], io.StringIO()), child_exit)
            run.assert_called_once_with(["cargo", "test"])
            self.assertIn("::warning::", output.getvalue())
            self.assertNotIn("fixture-secret", output.getvalue())

    def test_missing_log_skips_sampling_and_preserves_child_status(self):
        for child_exit in (0, 101):
            with self.subTest(child_exit=child_exit), \
                    patch.object(waits, "service_environment", return_value={}), \
                    patch.object(waits, "sample") as sample, \
                    patch.object(waits.subprocess, "run", return_value=subprocess.CompletedProcess([], child_exit)) as run, \
                    patch("sys.stdout", new_callable=io.StringIO) as output:
                self.assertEqual(waits.run(["cargo", "test"], None), child_exit)
            sample.assert_not_called()
            run.assert_called_once_with(["cargo", "test"])
            self.assertIn("::warning::", output.getvalue())

    def test_main_log_open_and_close_failures_preserve_child_status(self):
        for stage in ("open", "close"):
            for child_exit in (0, 101):
                with self.subTest(stage=stage, child_exit=child_exit), \
                        patch("sys.argv", ["ci-db-waits.py", "--log", "fixture.jsonl", "--", "cargo", "test"]), \
                        patch.object(waits, "service_environment", return_value={}), \
                        patch.object(waits, "sample"), \
                        patch.object(waits.subprocess, "run", return_value=subprocess.CompletedProcess([], child_exit)) as run, \
                        patch("sys.stdout", new_callable=io.StringIO) as output:
                    if stage == "open":
                        with patch.object(Path, "open", side_effect=OSError("fixture-secret")):
                            self.assertEqual(waits.main(), child_exit)
                    else:
                        log = io.StringIO()
                        with patch.object(Path, "open", return_value=log), \
                                patch.object(log, "close", side_effect=OSError("fixture-secret")):
                            self.assertEqual(waits.main(), child_exit)
                run.assert_called_once_with(["cargo", "test"])
                self.assertIn("::warning::", output.getvalue())
                self.assertNotIn("fixture-secret", output.getvalue())

    def test_advisory_observation_does_not_bypass_service_guard(self):
        for log in (None, io.StringIO()):
            with self.subTest(log=log), patch.dict(os.environ, {}, clear=True), \
                    patch.object(waits.subprocess, "run") as run:
                with self.assertRaisesRegex(RuntimeError, "ephemeral check job"):
                    waits.run(["cargo", "test"], log)
            run.assert_not_called()


class WorkflowContractTests(unittest.TestCase):
    def setUp(self):
        self.workflow = (Path(__file__).resolve().parents[1] / ".github/workflows/check.yml").read_text()

    def step(self, name):
        return self.workflow.split(f"      - name: {name}\n", 1)[1].split("\n      - ", 1)[0]

    def test_observed_suites_keep_exact_commands_and_default_concurrency(self):
        suites = [
            ("cargo test (unit and binary, including RSVP store)", "unit-db-waits.jsonl",
             "cargo test --workspace --lib --bins --features two-bot-core/db --locked"),
            ("cargo test (integration, including website acceptance)", "integration-db-waits.jsonl",
             "cargo test --workspace --test '*' --features two-bot-core/db --locked"),
            ("Internal-member durable store regressions (CI service only)", "internal-member-db-waits.jsonl",
             "cargo test -p two-bot-discord --features db --locked --test internal_member_store -- --ignored"),
        ]
        for name, log, command in suites:
            with self.subTest(name=name):
                step = self.step(name)
                run = step.split("        run: >-\n", 1)[1]
                argv = shlex.split(" ".join(line.strip() for line in run.splitlines()
                                           if line.startswith("          ")))
                self.assertEqual(argv, ["python3", "scripts/ci-db-waits.py", "--log",
                                        f"$RUNNER_TEMP/ci-db-waits/{log}", "--"] + shlex.split(command))
                self.assertIn("TWO_TEST_DATABASE_URL: postgres://agent_test:@agent-testdb:5432/two_bot_test_ci", step)
                self.assertNotIn("continue-on-error", step)
                self.assertNotIn("--test-threads", step)

    def test_wait_evidence_upload_uses_one_scoped_directory_after_failure(self):
        step = self.step("Preserve CI database wait evidence")
        self.assertIn("if: ${{ always() }}", step)
        self.assertIn("name: ci-db-waits-${{ github.run_id }}-${{ github.run_attempt }}", step)
        self.assertIn("          path: ${{ runner.temp }}/ci-db-waits\n", step)
        self.assertNotIn("path: |", step)
        self.assertIn("retention-days: 7", step)

    def test_wait_evidence_directory_is_prepared_before_observation(self):
        name = "Prepare CI database wait evidence"
        self.assertIn('run: mkdir -p "$RUNNER_TEMP/ci-db-waits"', self.step(name))
        self.assertLess(self.workflow.index(f"- name: {name}"),
                        self.workflow.index("- name: cargo test (unit and binary"))

    def test_single_upload_path_collects_available_container_logs(self):
        step = self.step("Preserve CI database wait evidence")
        path = step.split("          path: ", 1)[1].splitlines()[0]
        names = ["unit-db-waits.jsonl", "integration-db-waits.jsonl", "internal-member-db-waits.jsonl"]
        # The runner translates an action input's initial host path, not each
        # line. Exercise both an early failed suite and a complete observation.
        for available in (names[:1], names):
            with self.subTest(available=available), \
                    tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")) as root:
                host_temp = Path(root) / "host-temp"
                container_temp = Path(root) / "container-temp"
                logs = container_temp / "ci-db-waits"
                logs.mkdir(parents=True)
                for name in available:
                    (logs / name).write_text("{}\n")
                (container_temp / "unrelated.jsonl").write_text("not evidence\n")
                translated = path.replace("${{ runner.temp }}", str(host_temp))
                if translated.startswith(str(host_temp) + "/"):
                    translated = str(container_temp) + translated[len(str(host_temp)):]
                found = sorted(p.name for line in translated.splitlines() for p in Path(line).rglob("*.jsonl"))
                self.assertEqual(found, sorted(available))

    def test_offline_observer_regressions_precede_observed_tests(self):
        name = "- name: Offline CI wait-observer regressions"
        self.assertEqual(self.workflow.count(name), 1)
        self.assertLess(self.workflow.index(name), self.workflow.index("- name: cargo test (unit and binary"))

    def test_observation_preserves_suite_and_exact_image_gates(self):
        workflow = self.workflow
        self.assertIn("cargo test -p two-bot-discord --features db --locked --test internal_member_store -- --ignored", workflow)
        self.assertIn("python3 scripts/ci-db-waits.py --log", workflow)
        self.assertNotIn("internal_member_store -- --ignored --test-threads=1", workflow)
        self.assertIn("tags: two-bot:ci-${{ github.run_id }}-${{ github.run_attempt }}", workflow)
        self.assertEqual(workflow.count("RUNTIME_IMAGE: ${{ steps.runtime-image.outputs.imageid }}"), 3)
        self.assertIn('python3 scripts/container-smoke.py "$RUNTIME_IMAGE"', workflow)
        self.assertIn('"--$budget-max-bytes" 1', workflow)
        self.assertIn("for budget in image binary", workflow)
        self.assertNotIn("scripts/container-smoke.py two-bot:ci", workflow)


if __name__ == "__main__":
    unittest.main()
