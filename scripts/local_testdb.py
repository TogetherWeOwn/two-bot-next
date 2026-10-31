#!/usr/bin/env python3
"""Run the DB-backed Rust suites locally against a disposable Postgres.

Mirrors the `rust-tests` job in .github/workflows/check.yml: the same digest-
pinned `postgres` service aliased `agent-testdb:5432` (trust auth, tmpfs data,
bounded WAL), the same bootstrap databases and test environment, and the same
`rust:bookworm` job image. The test guard accepts only the literal host
`agent-testdb`, so Cargo runs in a runner container on the same private Docker
network rather than on the host. Nothing here touches staging, production or
host Postgres, and no secret is read.

  python3 scripts/local_testdb.py test            # fresh DB + full rust-tests sequence + summary
  python3 scripts/local_testdb.py up [--fresh]    # start/prepare only (idempotent)
  python3 scripts/local_testdb.py exec [--fresh] -- cargo test -p two-bot-core --features db --locked
  python3 scripts/local_testdb.py env             # print the test environment
  python3 scripts/local_testdb.py down [--volumes]

Each checkout gets its own network, containers and target volume, so parallel
worktrees do not share a database. The Cargo/rustup cache volume is shared.
"""

import argparse
import hashlib
import os
from pathlib import Path
import re
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]
CHECK = ROOT / ".github/workflows/check.yml"
INSTANCE = "two-localtest-" + hashlib.sha256(str(ROOT).encode()).hexdigest()[:10]
NETWORK, DB, RUNNER = INSTANCE, f"{INSTANCE}-db", f"{INSTANCE}-runner"
TARGET_VOLUME, CACHE_VOLUME = f"{INSTANCE}-target", "two-localtest-cache"

BOOTSTRAP = "postgres://agent_test:@agent-testdb:5432/two_bot_test_ci"
ADMIN = "postgres://agent_test@agent-testdb:5432/agent_test"
DATABASES = ("two_bot_test_ci", "two_next_backup_ci", "two_next_backup_publication_ci")
WAL = (("max_wal_size", "128MB"), ("min_wal_size", "32MB"))
# CI's 1 GiB tmpfs fits a CI runner's test parallelism. A many-core workstation
# keeps more fixture databases alive at once and fills it (ENOSPC), so default
# larger here. The tmpfs counts against the container memory cap.
TMPFS_GIB = int(os.environ.get("TWO_LOCAL_TESTDB_TMPFS_GIB", "4"))

# The rust-tests job env in check.yml. DB URLs are step-scoped there, as here.
JOB_ENV = {
    "TWO_CUSTOM_COMMAND_TEST_CI": "1",
    "TWO_AUTOMATION_TRANSFER_TEST_CI": "1",
    "CARGO_PROFILE_DEV_DEBUG": "line-tables-only",
    "CARGO_PROFILE_TEST_DEBUG": "line-tables-only",
    "CARGO_PROFILE_DEV_INCREMENTAL": "false",
    "CARGO_PROFILE_TEST_INCREMENTAL": "false",
    "TWO_LFG_TESTDB_CI": "1",
    "TWO_GATEWAY_TEST_DATABASE_URL": "postgresql://agent_test@localhost:5432/agent_test",
    "TWO_DATABASE_TLS": "local-only",
}
DB_ENV = {"TWO_TEST_DATABASE_URL": BOOTSTRAP}
INTEGRATION_ENV = dict(
    DB_ENV, TWO_RSVP_RUNTIME_TEST_DATABASE_URL=BOOTSTRAP,
    TWO_BOT_TEST_DATABASE_URL="postgres://agent_test:@agent-testdb:5432/two_next_backup_ci")
RUNNER_ENV = {
    "HOME": "/cache/home",
    "CARGO_HOME": "/cache/cargo",
    "RUSTUP_HOME": "/cache/rustup",
    "CARGO_TARGET_DIR": "/target",
    "PAPERCLIP_RUN_SCRATCH_DIR": "/target/scratch",
    "PATH": "/cache/cargo/bin:/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
}

# The rust-tests job's test steps, in order, with their step-level env.
STEPS = (
    ("generate command reference", {},
     "python3 scripts/generate_command_reference.py --verify-lifecycle"),
    ("unit and binary", DB_ENV,
     "cargo test --workspace --lib --bins --features two-bot-core/db --locked -- "
     "--skip ticket_runtime::cursor_budget --skip settings_jobs::tests::ops_overrides_acceptance_tests::"),
    ("isolated ops override acceptance", DB_ENV,
     "cargo test -p two-bot --bin two-bot --features two-bot-core/db --locked "
     "settings_jobs::tests::ops_overrides_acceptance_tests:: -- --test-threads=1"),
    ("isolated voice-name filter acceptance", {},
     "cargo test -p two-bot --lib --features two-bot-core/db --locked "
     "voice_rooms::tests::name_filter_acceptance -- --test-threads=1"),
    ("isolated ticket skip/budget acceptance", {},
     "cargo test -p two-bot --bin two-bot --features two-bot-core/db --locked "
     "ticket_runtime::cursor_budget -- --test-threads=1"),
    ("integration", INTEGRATION_ENV,
     "cargo test --workspace --test '*' --features two-bot-core/db --locked -- "
     "--skip cli_write_failure_and_crash_never_publish_prune_or_upload_a_partial_dump "
     "--skip seeded_aggregates_render_exactly_guild_scoped_and_read_only "
     "--skip uses_one_above_i32_max_saturates_and_write_proceeds "
     "--skip uses_u64_max_saturates_at_i32_max "
     "--skip saturated_write_warns_with_identifiers_and_no_raw_counter "
     "--skip normal_uses_round_trip_byte_identical "
     "--skip exact_i32_max_writes_without_saturation_warn"),
    ("isolated invite saturation acceptance", DB_ENV,
     "cargo test -p two-bot-store --test invite_uses_saturation_acceptance "
     "--features two-bot-core/db --locked -- --test-threads=1"),
    ("docs", {}, "cargo test --workspace --doc --features two-bot-core/db --locked"),
)
RESULT = re.compile(r"test result: \w+\. (\d+) passed; (\d+) failed; (\d+) ignored")


def images():
    """Return the (postgres, rust) images pinned by check.yml's rust-tests job."""
    text = CHECK.read_text()
    job = text[text.index("\n  rust-tests:\n"):]
    job = job[:re.search(r"\n  [a-z][a-z0-9-]*:\n", job[1:]).start() + 1]
    found = {}
    for name in ("postgres", "rust"):
        pins = set(re.findall(rf"image: ({name}:[\w.-]+@sha256:[0-9a-f]{{64}})", job))
        if len(pins) != 1:
            raise SystemExit(f"expected one pinned {name} image in rust-tests, found {sorted(pins)}")
        found[name] = pins.pop()
    return found["postgres"], found["rust"]


def docker(*args, check=True, capture=False):
    return subprocess.run(["docker", *args], check=check, text=True,
                          stdout=subprocess.PIPE if capture else None,
                          stderr=subprocess.PIPE if capture else None)


def exists(kind, name):
    return docker(kind, "inspect", name, check=False, capture=True).returncode == 0


def running(name):
    result = docker("inspect", "-f", "{{.State.Running}}", name, check=False, capture=True)
    return result.returncode == 0 and result.stdout.strip() == "true"


def git_mounts():
    """Bind the checkout at its host path; a worktree also needs its common git dir."""
    paths = [ROOT]
    common = subprocess.run(["git", "rev-parse", "--path-format=absolute", "--git-common-dir"],
                            cwd=ROOT, capture_output=True, text=True, check=False)
    if common.returncode == 0:
        git_dir = Path(common.stdout.strip())
        if not git_dir.is_relative_to(ROOT):
            paths.append(git_dir)
    return [arg for path in paths for arg in ("-v", f"{path}:{path}")]


def runner_exec(command, env=None, root=False, check=True):
    user = "0:0" if root else f"{os.getuid()}:{os.getgid()}"
    merged = dict(RUNNER_ENV, **(env or {}))
    flags = [arg for key, value in merged.items() for arg in ("-e", f"{key}={value}")]
    return docker("exec", "-u", user, "-w", str(ROOT), *flags, RUNNER, *command, check=check)


def psql(url, *sql, capture=False):
    args = ["psql", url, "-X", "-w", "-v", "ON_ERROR_STOP=1", "-At"]
    for statement in sql:
        args += ["-c", statement]
    if capture:
        user = f"{os.getuid()}:{os.getgid()}"
        return docker("exec", "-u", user, RUNNER, *args, capture=True).stdout
    return runner_exec(args)


def toolchain():
    match = re.search(r'^channel\s*=\s*"([^"]+)"', (ROOT / "rust-toolchain.toml").read_text(), re.M)
    if not match:
        raise SystemExit("rust-toolchain.toml has no channel")
    return match.group(1)


def up(fresh=False):
    """Start or reuse the stack; fresh replaces the database like a new CI job."""
    postgres, rust = images()
    if fresh:
        docker("rm", "-f", DB, check=False, capture=True)
    if not exists("network", NETWORK):
        docker("network", "create", NETWORK, capture=True)
    if not running(DB):
        docker("rm", "-f", DB, check=False, capture=True)
        # Same service shape as check.yml (trust auth, tmpfs PGDATA, memory cap),
        # sized by TMPFS_GIB instead of CI's 1 GiB.
        tmpfs, memory = TMPFS_GIB << 30, (TMPFS_GIB + 1) << 30
        docker("run", "-d", "--name", DB, "--network", NETWORK, "--network-alias", "agent-testdb",
               "-e", "POSTGRES_USER=agent_test", "-e", "POSTGRES_DB=agent_test",
               "-e", "POSTGRES_HOST_AUTH_METHOD=trust",
               "--tmpfs", f"/var/lib/postgresql:rw,size={tmpfs}",
               "--memory", str(memory), "--memory-swap", str(memory), postgres, capture=True)
    if not running(RUNNER):
        docker("rm", "-f", RUNNER, check=False, capture=True)
        docker("run", "-d", "--name", RUNNER, "--network", NETWORK,
               "-v", f"{TARGET_VOLUME}:/target", "-v", f"{CACHE_VOLUME}:/cache",
               *git_mounts(), "--entrypoint", "sleep", rust, "infinity", capture=True)
        runner_exec(["sh", "-c",
                     "apt-get update -qq && apt-get install -y -qq --no-install-recommends "
                     "python3 postgresql-client socat >/dev/null && rm -rf /var/lib/apt/lists/* && "
                     "mkdir -p /cache/home /cache/cargo /cache/rustup /target/scratch && "
                     f"chown {os.getuid()}:{os.getgid()} /cache /cache/home /cache/cargo "
                     "/cache/rustup /target /target/scratch"], root=True)
        runner_exec(["rustup", "toolchain", "install", toolchain(), "--profile", "minimal",
                     "--component", "rustfmt", "--component", "clippy", "--no-self-update"])
        # Tests that pin localhost:5432 reach the service through this forward, as in CI.
        docker("exec", "-d", RUNNER, "socat",
               "TCP-LISTEN:5432,bind=127.0.0.1,reuseaddr,fork", "TCP:agent-testdb:5432")
    # Probe over the network like CI: the image's init server answers on the
    # unix socket before it listens on TCP.
    for _ in range(60):
        probe = docker("exec", RUNNER, "pg_isready", "-q", "-h", "agent-testdb", "-p", "5432",
                       "-U", "agent_test", "-d", "agent_test", check=False, capture=True)
        if probe.returncode == 0:
            break
        time.sleep(1)
    else:
        raise SystemExit("disposable Postgres did not become healthy")
    present = set(psql(ADMIN, "SELECT datname FROM pg_database", capture=True).split())
    if not set(DATABASES) <= present:
        psql(ADMIN, *(f"ALTER SYSTEM SET {key} = '{value}'" for key, value in WAL),
             "SELECT pg_reload_conf()")
        for name in DATABASES:
            if name not in present:
                runner_exec(["createdb", "-h", "agent-testdb", "-U", "agent_test", name])
    print(f"ready: {DB} as agent-testdb:5432 on network {NETWORK}; runner {RUNNER}")


def down(volumes):
    for name in (RUNNER, DB):
        docker("rm", "-f", name, check=False, capture=True)
    docker("network", "rm", NETWORK, check=False, capture=True)
    if volumes:
        docker("volume", "rm", TARGET_VOLUME, check=False, capture=True)
    print(f"removed {INSTANCE}" + (" and its target volume" if volumes else ""))


def run_step(env, command):
    """Stream one step's output and return (exit code, passed, failed, ignored)."""
    user = f"{os.getuid()}:{os.getgid()}"
    merged = dict(RUNNER_ENV, **JOB_ENV, **env)
    flags = [arg for key, value in merged.items() for arg in ("-e", f"{key}={value}")]
    process = subprocess.Popen(
        ["docker", "exec", "-u", user, "-w", str(ROOT), *flags, RUNNER, "bash", "-c", command],
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, errors="replace")
    totals = [0, 0, 0]
    for line in process.stdout:
        sys.stdout.write(line)
        match = RESULT.search(line)
        if match:
            totals = [total + int(count) for total, count in zip(totals, match.groups())]
    return (process.wait(), *totals)


def test():
    # Leftover fixture databases and WAL from an earlier run can exhaust the
    # tmpfs, so every full run starts on an empty service as CI does.
    up(fresh=True)
    rows = []
    for name, env, command in STEPS:
        # CI stops at the first failing test binary; locally, report them all.
        if command.startswith("cargo test "):
            command = command.replace("cargo test ", "cargo test --no-fail-fast ", 1)
        print(f"\n==> {name}: {command}", flush=True)
        rows.append((name, *run_step(env, command)))
    print("\nstep                                      exit  passed  failed  ignored")
    for name, code, passed, failed, ignored in rows:
        print(f"{name:<40}  {code:>4}  {passed:>6}  {failed:>6}  {ignored:>7}")
    totals = [sum(row[index] for row in rows) for index in (2, 3, 4)]
    print(f"{'total':<40}  {'':>4}  {totals[0]:>6}  {totals[1]:>6}  {totals[2]:>7}")
    return 1 if any(row[1] for row in rows) else 0


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest="command", required=True)
    start = commands.add_parser("up", help="start and prepare the disposable database and runner")
    start.add_argument("--fresh", action="store_true", help="replace the database with an empty one")
    commands.add_parser("test", help="up, then run the rust-tests sequence and summarize")
    commands.add_parser("env", help="print the test environment used inside the runner")
    run = commands.add_parser("exec", help="run a command in the runner with the test environment")
    run.add_argument("--fresh", action="store_true", help="replace the database with an empty one")
    run.add_argument("argv", nargs=argparse.REMAINDER)
    stop = commands.add_parser("down", help="remove the containers and network")
    stop.add_argument("--volumes", action="store_true", help="also remove this checkout's target volume")
    args = parser.parse_args(argv)
    if args.command == "up":
        up(args.fresh)
    elif args.command == "test":
        return test()
    elif args.command == "env":
        for key, value in dict(JOB_ENV, **INTEGRATION_ENV).items():
            print(f"{key}={value}")
    elif args.command == "exec":
        command = args.argv[1:] if args.argv[:1] == ["--"] else args.argv
        if not command:
            parser.error("exec needs a command after --")
        up(args.fresh)
        return runner_exec(command, env=dict(JOB_ENV, **INTEGRATION_ENV), check=False).returncode
    elif args.command == "down":
        down(args.volumes)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
