"""Run exactly one default/serial pair on the job's prebuilt synthetic fixture."""

import hashlib
import os
import re
import subprocess
import tempfile
import time
from pathlib import Path

CASES = "sigterm|decoy|bootstrap_decoy|collision|lost_ack|teardown"
PHASES = (
    "test_threads|test_enter|test_leave|schema_ready|boot_start|health_ok|ready_starting|"
    "auth_begin|auth_done|ready_begin|ready_ok|job_state|tick_begin|tick_hit|tick_miss|"
    "mock_tcp_accept|mock_upgrade_done|mock_rest_identity|mock_rest_user|mock_rest_guild|"
    "mock_rest_registry|mock_rest_events|lifecycle_begin|lifecycle_pass|"
    "lifecycle_timeout|lifecycle_assertion|teardown_case|teardown_begin|teardown_health_ok|"
    "teardown_upgrade_timeout|teardown_health_timeout|teardown_injected_timeout|"
    "teardown_assertion|teardown_pass"
)
TRACE = re.compile(
    rf"\balive_observe (?:{CASES}) (?:{PHASES}) [0-9]{{1,16}} [0-9]{{1,16}}$"
)
SUMMARY = re.compile(
    r"test result: (?:ok|FAILED)\. [0-9]+ passed; [0-9]+ failed; "
    r"[0-9]+ ignored; [0-9]+ measured; [0-9]+ filtered out; finished in [0-9.]+s"
)
FAILURE = {
    "events job never hit REST during first boot": "scheduled_events_receipt_timeout",
    "authentication deadline: Elapsed(())": "auth_timeout",
    "/healthz did not return 200 before deadline": "health_timeout",
    "/readyz did not return 503 before deadline": "readiness_starting_timeout",
    "/readyz did not return 200 before deadline": "readiness_ready_timeout",
}
ACTIVITY = """
SELECT count(*), count(*) FILTER (WHERE state = 'active'),
       count(*) FILTER (WHERE wait_event_type = 'Lock'),
       count(*) FILTER (WHERE wait_event_type = 'IO')
FROM pg_stat_activity
WHERE datname = 'agent_test' AND pid <> pg_backend_pid()
"""


def emit(label, **fields):
    print(label, *(f"{name}={value}" for name, value in fields.items()), flush=True)


def sample_pressure(origin):
    fields = {"pair_ms": (time.monotonic_ns() - origin) // 1_000_000}
    fields["load1"] = os.getloadavg()[0]
    try:
        cpu = dict(
            row.split()
            for row in Path("/sys/fs/cgroup/cpu.stat").read_text().splitlines()
        )
        for key in ("usage_usec", "nr_throttled", "throttled_usec"):
            if key in cpu:
                fields[f"cpu_{key}"] = int(cpu[key])
    except (OSError, ValueError):
        fields["cpu_counters"] = "unavailable"
    try:
        result = subprocess.check_output(
            [
                "psql",
                "-XwAt",
                "-v",
                "ON_ERROR_STOP=1",
                "-h",
                "agent-testdb",
                "-U",
                "agent_test",
                "-d",
                "agent_test",
                "-c",
                ACTIVITY,
            ],
            env=os.environ
            | {
                "PGPASSFILE": "/dev/null",
                "PGSSLMODE": "disable",
                "PGOPTIONS": "-c statement_timeout=400",
            },
            stderr=subprocess.DEVNULL,
            text=True,
            timeout=0.6,
        )
        sessions, active, lock, io = map(int, result.strip().split("|"))
        fields.update(db_sessions=sessions, db_active=active, db_lock=lock, db_io=io)
    except (OSError, ValueError, subprocess.SubprocessError):
        fields["db_activity"] = "unavailable"
    emit("pressure", **fields)


def publish(log):
    # Do not forward child logs, raw panic/value text, or even unknown test output.
    log.seek(0)
    for line in log:
        line = line.strip()
        match = TRACE.search(line)
        if match:
            print(match[0], flush=True)
        elif SUMMARY.fullmatch(line):
            print(line, flush=True)
        elif line in FAILURE:
            emit("fixture_failure", kind=FAILURE[line])


def built_inputs():
    app = Path("target/debug/two-bot")
    tests = [
        p
        for p in app.parent.joinpath("deps").glob("alive-*")
        if p.is_file() and not p.suffix and os.access(p, os.X_OK)
    ]
    if len(tests) != 1 or not app.is_file():
        raise SystemExit("prebuild did not yield one app and one alive test executable")
    digests = []
    for binary in (app, tests[0]):
        with binary.open("rb") as stream:
            digests.append(hashlib.file_digest(stream, "sha256").hexdigest())
    return tuple(digests)


def check_inputs(label):
    try:
        return built_inputs()
    except (OSError, SystemExit):
        emit("input_check", after=label, result="unavailable")
        return None


def run_sample(label, origin, extra):
    emit(
        "sample_begin",
        name=label,
        pair_ms=(time.monotonic_ns() - origin) // 1_000_000,
        command_budget_ms=60_000,
        libtest_threads=1 if extra else "default",
    )
    command = [
        "timeout",
        "60s",
        "cargo",
        "test",
        "-p",
        "two-bot",
        "--locked",
        "--test",
        "alive",
        "--",
        "--ignored",
        "--nocapture",
        *extra,
    ]
    started = time.monotonic_ns()
    with tempfile.TemporaryFile(mode="w+t", encoding="utf-8", errors="replace") as log:
        with subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT) as child:
            while child.poll() is None:
                sample_pressure(origin)
                try:
                    child.wait(timeout=1)
                except subprocess.TimeoutExpired:
                    pass
        code = child.returncode
        emit(
            "sample_verdict",
            name=label,
            exit_code=code,
            elapsed_ms=(time.monotonic_ns() - started) // 1_000_000,
        )
        publish(log)
    return code


def main():
    if "RUST_TEST_THREADS" in os.environ or "CARGO_TARGET_DIR" in os.environ:
        raise SystemExit("unexpected ambient thread or target override")
    before = built_inputs()
    emit("inputs", app_sha256=before[0], alive_sha256=before[1])
    origin = time.monotonic_ns()
    normal = run_sample("normal", origin, [])
    between = check_inputs("normal")
    control = run_sample("serial", origin, ["--test-threads=1"])
    after = check_inputs("serial")
    emit(
        "pair_verdict",
        normal=normal,
        serial=control,
        same_inputs=int(before == between == after),
    )
    return int(bool(normal or control or before != between or before != after))


if __name__ == "__main__":
    raise SystemExit(main())
