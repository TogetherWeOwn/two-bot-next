"""Record DDL waits on the isolated check job's PostgreSQL service, not test SQL."""

import argparse
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import subprocess
import threading
import time

# I/O counters can lag live waits; zero timing with tracking off is not no I/O.
# Record only allowlisted settings of this read-only observer, never change them.
QUERY = """
WITH ddl AS (
  SELECT pid, datname, state, query_start, wait_event_type, wait_event,
    CASE WHEN query LIKE 'CREATE DATABASE%' THEN 'create' ELSE 'drop' END AS operation,
    substring(query FROM '^(?:CREATE|DROP) DATABASE "(two_bot_test_[0-9a-f]+_[0-9a-f]+_[0-9a-f]+)"') AS target
  FROM pg_stat_activity
  WHERE pid <> pg_backend_pid() AND state = 'active'
    AND (query LIKE 'CREATE DATABASE%' OR query LIKE 'DROP DATABASE%')
)
SELECT json_build_object(
  'at', clock_timestamp(),
  'ddl', COALESCE((SELECT json_agg(json_build_object(
    'pid', pid, 'bootstrap', datname, 'state', state, 'operation', operation,
    'target', target, 'elapsed_ms', extract(epoch FROM clock_timestamp() - query_start) * 1000,
    'wait_type', wait_event_type, 'wait_event', wait_event,
    'blockers', pg_blocking_pids(pid),
    'target_sessions', (SELECT count(*) FROM pg_stat_activity a WHERE a.datname = ddl.target),
    'target_catalog', (SELECT json_build_object('oid', oid, 'connection_limit', datconnlimit)
      FROM pg_database d WHERE d.datname = ddl.target))) FROM ddl), '[]'::json),
  'checkpointer', (SELECT row_to_json(c) FROM (
    SELECT num_requested, num_done, write_time, sync_time, buffers_written, stats_reset
    FROM pg_stat_checkpointer
  ) c),
  'checkpointer_activity', COALESCE((SELECT json_agg(row_to_json(c)) FROM (
    SELECT pid, state, wait_event_type AS wait_type, wait_event
    FROM pg_stat_activity WHERE backend_type = 'checkpointer'
  ) c), '[]'::json),
  'checkpointer_io', COALESCE((SELECT json_agg(row_to_json(c)) FROM (
    SELECT object, context, writes, write_time, writebacks, writeback_time,
      fsyncs, fsync_time, stats_reset
    FROM pg_stat_io WHERE backend_type = 'checkpointer'
  ) c), '[]'::json),
  'observer_settings', json_build_object(
    'server_version_num', current_setting('server_version_num'),
    'track_io_timing', current_setting('track_io_timing'),
    'track_wal_io_timing', current_setting('track_wal_io_timing'),
    'fsync', current_setting('fsync'),
    'full_page_writes', current_setting('full_page_writes'),
    'synchronous_commit', current_setting('synchronous_commit'),
    'wal_sync_method', current_setting('wal_sync_method'),
    'checkpoint_timeout', current_setting('checkpoint_timeout'),
    'checkpoint_completion_target', current_setting('checkpoint_completion_target'),
    'checkpoint_flush_after', current_setting('checkpoint_flush_after')));
"""


def service_environment():
    # No inherited PG credentials, service file, production URL or psql startup file.
    # This observer is for the job-container service only, never the controller.
    if os.environ.get("GITHUB_ACTIONS") != "true" or os.environ.get("TWO_LFG_TESTDB_CI") != "1":
        raise RuntimeError("DB wait observer requires the ephemeral check job")
    return {
        "PATH": os.environ["PATH"], "LC_ALL": "C",
        "PGHOST": "agent-testdb", "PGPORT": "5432", "PGUSER": "agent_test",
        "PGDATABASE": "two_bot_test_ci", "PGPASSWORD": "", "PGPASSFILE": "/dev/null",
        "PGSERVICEFILE": "/dev/null", "PGCONNECT_TIMEOUT": "2",
        "PGOPTIONS": "-c statement_timeout=1000 -c default_transaction_read_only=on",
    }


def stderr_hint(stderr):
    # Fixed labels are hints, not root causes; never return any supplied text.
    text = (stderr or "").lower()
    for hint, fragments in (
        ("name-resolution", ("could not translate host name",)),
        ("connection-refused", ("connection refused",)),
        ("connect-timeout", ("timeout expired",)),
        ("authentication-rejected", ("password authentication failed", "no pg_hba.conf entry")),
        ("connection-limit", ("remaining connection slots", "too many clients already")),
        ("connection-lost", ("server closed the connection unexpectedly", "connection to server was lost",
                             "terminating connection due to administrator command")),
        ("server-unavailable", ("database system is starting up", "database system is shutting down",
                               "database system is in recovery mode")),
        ("statement-timeout", ("canceling statement due to statement timeout",)),
    ):
        if any(fragment in text for fragment in fragments):
            return hint
    return "unclassified"


def record_failure(log, started, hint, psql_exit=None):
    # A client-clock failure receipt is not a successful server sample.
    log.write(json.dumps({"observer_error": {
        "client_at": datetime.now(timezone.utc).isoformat(),
        "elapsed_ms": round((time.monotonic() - started) * 1000, 3),
        "psql_exit": psql_exit, "stderr_hint": hint,
    }}) + "\n")
    log.flush()


def sample(environment, log):
    started = time.monotonic()
    try:
        result = subprocess.run(
            ["psql", "--no-psqlrc", "--no-password", "--tuples-only", "--no-align",
             "--set", "ON_ERROR_STOP=1", "--command", QUERY],
            env=environment, capture_output=True, text=True, timeout=3,
        )
    except subprocess.TimeoutExpired:
        record_failure(log, started, "client-process-timeout")
        raise RuntimeError("CI service wait query exceeded process bound") from None
    except OSError:
        record_failure(log, started, "client-process-error")
        raise RuntimeError("CI service wait query process failed") from None
    # Fail on this credential/connection, never retry with another one. Do not
    # copy arbitrary psql stderr into evidence (it can echo connection input).
    if result.returncode:
        hint = stderr_hint(result.stderr)
        record_failure(log, started, hint, result.returncode)
        raise RuntimeError(f"CI service wait query failed (psql exit {result.returncode}; {hint})")
    record = json.loads(result.stdout)
    log.write(json.dumps(record) + "\n")
    log.flush()


def run(command, log):
    environment = service_environment()
    stopped = threading.Event()
    failed = threading.Event()
    if log is None:
        failed.set()
    else:
        try:
            sample(environment, log)
        except (RuntimeError, ValueError, OSError, subprocess.SubprocessError):
            failed.set()

    def observe():
        while not stopped.wait(0.25):
            try:
                sample(environment, log)
            except (RuntimeError, ValueError, OSError, subprocess.SubprocessError):
                failed.set()
                return

    observer = None
    if not failed.is_set():
        observer = threading.Thread(target=observe)
        try:
            observer.start()
        except (RuntimeError, OSError):
            failed.set()
            observer = None
    try:
        # Observation is advisory; only the unchanged suite determines its status.
        result = subprocess.run(command)
    finally:
        stopped.set()
        if observer is not None:
            observer.join()
    if failed.is_set():
        print("::warning::DB wait evidence incomplete; sampling stopped", flush=True)
    return result.returncode


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--log", required=True, type=Path)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command:
        parser.error("a test command is required")
    try:
        log = args.log.open("w")
    except OSError:
        log = None
    try:
        return run(command, log)
    except (RuntimeError, ValueError, OSError, subprocess.SubprocessError) as error:
        parser.exit(1, f"DB wait observer failed: {error}\n")
    finally:
        if log is not None:
            try:
                log.close()
            except OSError:
                print("::warning::DB wait evidence incomplete; log close failed", flush=True)


if __name__ == "__main__":
    raise SystemExit(main())
