#!/usr/bin/env python3
"""Run one CI test-lane partition against the disposable CI database service.

Integration targets are discovered from Cargo metadata, not a checked-in list.
The other commands are the intentionally filtered/isolated baseline replays.
"""

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import shlex
import signal
import subprocess
import sys
import time

from cargo_feature_selection import workspace_feature_args

BOOTSTRAP = "postgres://agent_test:@agent-testdb:5432/two_bot_test_ci"
EMPTY_PASSWORD = "postgres://agent_test:@agent-testdb:5432/agent_test"
EXACT = "postgres://agent_test@agent-testdb:5432/agent_test"
BACKUP = "postgres://agent_test:@agent-testdb:5432/two_next_backup_ci"
DB_KEYS = ("TWO_TEST_DATABASE_URL", "TWO_RSVP_RUNTIME_TEST_DATABASE_URL",
           "TWO_BOT_TEST_DATABASE_URL")
FEATURES = ["--features", "two-bot-core/db", "--locked"]
INTEGRATION_SKIPS = (
    "cli_write_failure_and_crash_never_publish_prune_or_upload_a_partial_dump",
    "seeded_aggregates_render_exactly_guild_scoped_and_read_only",
    "uses_one_above_i32_max_saturates_and_write_proceeds",
    "uses_u64_max_saturates_at_i32_max",
    "saturated_write_warns_with_identifiers_and_no_raw_counter",
    "normal_uses_round_trip_byte_identical",
    "exact_i32_max_writes_without_saturation_warn",
)
PARTITIONS = {
    "rust-tests": ("unit-bin", "integration-0", "integration-1", "integration-2", "replays"),
    "ignored-db-runtime": ("onboarding", "gateway", "runtime", "cutover", "other"),
}


def command(args, database=None, *, integration=False, timeout=None, budget=False):
    return (args, database, integration, timeout, budget)


def runtime_commands(partition):
    bot = ["test", "-p", "two-bot", "--locked"]
    include = ["--", "--include-ignored"]
    serial = ["--", "--ignored", "--test-threads=1"]
    if partition == "onboarding":
        return [command(bot + ["onboarding_"] + include, BOOTSTRAP)]
    if partition == "gateway":
        return [command(bot + ["gateway_tests"] + serial)]
    if partition == "runtime":
        return [
            command(bot + [name] + include, BOOTSTRAP)
            for name in ("raid_runtime_", "join_risk_runtime_")
        ] + [
            command(bot + ["--lib", "--bins", "containment_worker_"] + include, BOOTSTRAP),
            command(bot + ["--lib", "--bins", "hotpath_settings_test_support::queries::observer_sees_actual_rows_and_rejects_unbounded_control"] + include + ["--test-threads=1"], BOOTSTRAP),
            command(bot + ["--lib", "--bins", "fail_open_containment_acceptance"] + include),
            command(bot + ["--test", "containment_worker_soak"] + include),
            command(bot + ["--lib", "--bins", "containment_runtime_evidence_"] + include),
            command(bot + ["--lib", "--bins", "configured_database_reports_seeded_family_frontier"] + include, BOOTSTRAP),
            command(bot + ["command_runtime_tests"] + serial, BOOTSTRAP),
            command(bot + ["feed_jobs"] + serial, BOOTSTRAP),
        ]
    if partition == "cutover":
        base = ["test", "-p", "two-bot-cutover"]
        items = [
            ("raid_list_db", EMPTY_PASSWORD, True),
            ("reengagement_list_db", EMPTY_PASSWORD, True),
            ("game_activity_report_db", EMPTY_PASSWORD, False),
            ("settings_db", None, True),
            ("voice_rooms_store", None, True),
            ("voice_config_store", None, True),
            ("voice_create_admission_store", BOOTSTRAP, True),
            ("legacy_copy_db", None, True),
            ("legacy_verify_db", EXACT, True),
            ("legacy_tls_fence", EXACT, True),
            ("staging_migrate_db", EXACT, True),
        ]
        result = [command(base + ["--test", name, "--locked"] + (["--", "--ignored"] if ignored else []), db)
                  for name, db, ignored in items]
        result += [command(base + ["--locked", "--test", name, "--", "--ignored"])
                   for name in ("invite_store", "member_list_catchup")]
        return result + [command(base + ["--locked", "--lib", "--", "--ignored"])]
    if partition == "other":
        return [
            command(bot + ["--test", "alive", "--", "--ignored", "--nocapture"], timeout=60),
            command(bot + ["--test", "restore_drill", "--", "--ignored", "--nocapture"], BOOTSTRAP),
            command(bot + ["--bin", "two-bot", "scorecard_marks_only_captured_streams", "--", "--ignored"], BOOTSTRAP),
            command(bot + ["--bin", "two-bot", "member_join_fact_round_trip_dedupes", "--", "--ignored"], BOOTSTRAP),
            command(["test", "-p", "two-bot-discord", "--features", "db", "--locked", "--test", "community_facts_capture", "--", "--ignored", "--test-threads=1"], BOOTSTRAP),
        ]
    raise ValueError("unknown runtime partition")


def integration_commands(index):
    proc = subprocess.run(["cargo", "metadata", "--format-version", "1", "--locked",
                           "--features", "two-bot-core/db"],
                          check=True, stdout=subprocess.PIPE, text=True, timeout=180)
    graph = json.loads(proc.stdout)
    members = set(graph["workspace_members"])
    packages = [p for p in graph["packages"] if p["id"] in members]
    nodes = {node["id"]: node for node in graph["resolve"]["nodes"]}
    core = next((p for p in packages if p["name"] == "two-bot-core"), None)
    if core is None or not members.issubset(nodes):
        raise ValueError("Cargo metadata is missing workspace dependency resolution")
    by_package = {}
    for package in packages:
        for target in package["targets"]:
            if "test" not in target["kind"] or not target.get("test", True):
                continue
            required = target.get("required-features", target.get("required_features", []))
            if not set(required).issubset(nodes[package["id"]].get("features", [])):
                continue
            key = package["name"] + "/" + target["name"]
            owner = int.from_bytes(hashlib.sha256(key.encode()).digest()[:8], "big") % 3
            if owner == index:
                by_package.setdefault(package["name"], []).append(target["name"])
    result = []
    for name, targets in sorted(by_package.items()):
        feature_args = workspace_feature_args(graph, [name])
        selectors = [word for target in sorted(targets) for word in ("--test", target)]
        skips = [word for skip in INTEGRATION_SKIPS for word in ("--skip", skip)]
        base = ["test", "-p", name] + selectors + feature_args + ["--locked"]
        result.append(command(base + ["--"] + skips, BOOTSTRAP, integration=True))
        result.append(command(base + ["property_"], budget=True))
    return result


def rust_commands(partition):
    if partition.startswith("integration-"):
        return integration_commands(int(partition[-1]))
    if partition == "unit-bin":
        return [command(["test", "--workspace", "--lib", "--bins"] + FEATURES +
                        ["--", "--skip", "ticket_runtime::cursor_budget", "--skip",
                         "settings_jobs::tests::ops_overrides_acceptance_tests::"], BOOTSTRAP),
                command(["test", "--workspace", "--lib", "--bins"] + FEATURES + ["property_"], budget=True)]
    if partition == "replays":
        return [
            command(["test", "-p", "two-bot", "--bin", "two-bot"] + FEATURES +
                    ["settings_jobs::tests::ops_overrides_acceptance_tests::", "--", "--test-threads=1"], BOOTSTRAP),
            command(["test", "-p", "two-bot", "--lib"] + FEATURES +
                    ["voice_rooms::tests::name_filter_acceptance", "--", "--test-threads=1"]),
            command(["test", "-p", "two-bot", "--bin", "two-bot"] + FEATURES +
                    ["ticket_runtime::cursor_budget", "--", "--test-threads=1"]),
            command(["test", "-p", "two-bot-store", "--test", "invite_uses_saturation_acceptance"] + FEATURES +
                    ["--", "--test-threads=1"], BOOTSTRAP),
            command(["test", "--workspace", "--doc"] + FEATURES),
            command(["run", "-p", "two-bot-core", "--example", "metrics_rss", "--locked"]),
        ]
    raise ValueError("unknown Rust partition")


def environment(database, integration):
    env = os.environ.copy()
    for key in DB_KEYS:
        env.pop(key, None)
    if database is not None:
        env["TWO_TEST_DATABASE_URL"] = database
    if integration:
        env["TWO_RSVP_RUNTIME_TEST_DATABASE_URL"] = BOOTSTRAP
        env["TWO_BOT_TEST_DATABASE_URL"] = BACKUP
        env["PAPERCLIP_RUN_SCRATCH_DIR"] = os.path.join(env["RUNNER_TEMP"], "two-bot-backup-tests")
    return env


def remaining_timeout(index, args, timeout, deadlines):
    if timeout is None:
        return None
    if index not in deadlines:
        deadlines[index] = time.monotonic() + timeout
    remaining = deadlines[index] - time.monotonic()
    if remaining <= 0:
        raise subprocess.TimeoutExpired(["cargo"] + args, timeout)
    return remaining


def run_cargo(args, *, env, timeout=None, **kwargs):
    argv = ["cargo"] + args
    if timeout is None:
        return subprocess.run(argv, env=env, check=True, **kwargs)
    # Cargo/test binaries can spawn children. Isolate only this invocation so an
    # expired inventory/execution deadline kills its whole tree, not our runner.
    with subprocess.Popen(argv, env=env, start_new_session=True, **kwargs) as process:
        try:
            stdout, stderr = process.communicate(timeout=timeout)
        except BaseException:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.communicate()
            raise
        if process.returncode:
            raise subprocess.CalledProcessError(process.returncode, argv, stdout, stderr)
        return subprocess.CompletedProcess(argv, process.returncode, stdout, stderr)


def has_tests(commands, deadlines):
    # Probe the selected commands, not the workspace as a whole. This also fails
    # closed on compile failures and never treats an empty partition as success.
    for index, (args, database, integration, timeout, _) in enumerate(commands):
        if args[0] != "test" or "--doc" in args:
            continue
        probe = args + (["--list"] if "--" in args else ["--", "--list"])
        print("Inventory:", shlex.join(["cargo"] + probe), file=sys.stderr, flush=True)
        result = run_cargo(probe, env=environment(database, integration),
                           stdout=subprocess.PIPE, text=True,
                           timeout=remaining_timeout(index, args, timeout, deadlines))
        if any(line.endswith(": test") for line in result.stdout.splitlines()):
            return True
    return False


def execute(commands):
    if any(item[2] for item in commands):
        Path(environment(None, True)["PAPERCLIP_RUN_SCRATCH_DIR"]).mkdir(parents=True, exist_ok=True)
    # Timeboxed commands share one deadline across compilation/listing and execution.
    deadlines = {}
    if not has_tests(commands, deadlines):
        raise ValueError("partition selected no tests")
    property_seconds = 0.0
    for index, (args, database, integration, timeout, budget) in enumerate(commands):
        print("Running:", shlex.join(["cargo"] + args), flush=True)
        start = time.monotonic()
        run_cargo(args, env=environment(database, integration),
                  timeout=remaining_timeout(index, args, timeout, deadlines))
        if budget:
            property_seconds += time.monotonic() - start
    if any(item[4] for item in commands):
        print(f"Property partition: {property_seconds:.6f} seconds (budget <30)", flush=True)
        if property_seconds >= 30:
            raise ValueError("property partition exceeded its 30-second budget")
    return property_seconds


def write_receipt(lane, partition, seconds):
    directory = Path(os.environ["RUNNER_TEMP"]) / "test-partition-receipts"
    directory.mkdir(parents=True, exist_ok=True)
    path = directory / f"{lane}-{partition}.json"
    path.write_text(json.dumps({"lane": lane, "partition": partition,
                                "property_seconds": seconds}) + "\n", encoding="utf-8")


def verify_receipts(lane, directory):
    directory = Path(directory)
    expected = {f"{lane}-{partition}.json": partition for partition in PARTITIONS[lane]}
    entries = list(directory.iterdir())
    if {p.name for p in entries} != set(expected) or any(not p.is_file() or p.is_symlink() for p in entries):
        raise ValueError("receipt directory must contain exactly one file for each lane partition")
    total = 0.0
    property_owners = {"unit-bin", "integration-0", "integration-1", "integration-2"}
    for path in entries:
        receipt = json.loads(path.read_text(encoding="utf-8"))
        if (not isinstance(receipt, dict) or set(receipt) != {"lane", "partition", "property_seconds"}
                or receipt["lane"] != lane or receipt["partition"] != expected[path.name]):
            raise ValueError(f"invalid receipt identity: {path.name}")
        seconds = receipt["property_seconds"]
        if (isinstance(seconds, bool) or not isinstance(seconds, (int, float))
                or not math.isfinite(seconds) or seconds < 0):
            raise ValueError(f"invalid receipt duration: {path.name}")
        if (lane != "rust-tests" or receipt["partition"] not in property_owners) and seconds != 0:
            raise ValueError(f"unexpected property duration: {path.name}")
        total += seconds
    if lane == "rust-tests" and total >= 30:
        raise ValueError(f"property suite exceeded its 30-second budget: {total:.6f}")
    print(f"Verified {len(entries)} {lane} receipts; property suite {total:.6f} seconds")


def main():
    parser = argparse.ArgumentParser(allow_abbrev=False)
    parser.add_argument("--lane", required=True, action="append", choices=PARTITIONS)
    selection = parser.add_mutually_exclusive_group(required=True)
    selection.add_argument("--list", action="append_const", const=True)
    selection.add_argument("--partition", action="append")
    selection.add_argument("--verify-receipts", action="append")
    args = parser.parse_args()
    if len(args.lane) != 1 or (args.list is not None and len(args.list) != 1):
        parser.error("lane and selection must each be specified exactly once")
    lane = args.lane[0]
    if args.list:
        print(json.dumps({"include": [{"partition": p} for p in PARTITIONS[lane]]}, separators=(",", ":")))
        return
    if args.verify_receipts is not None:
        if len(args.verify_receipts) != 1 or not args.verify_receipts[0]:
            parser.error("receipt directory must be specified exactly once")
        verify_receipts(lane, args.verify_receipts[0])
        return
    if len(args.partition) != 1 or args.partition[0] not in PARTITIONS[lane]:
        parser.error("partition must be specified once and belong to the selected lane")
    partition = args.partition[0]
    receipt = Path(os.environ["RUNNER_TEMP"]) / "test-partition-receipts" / f"{lane}-{partition}.json"
    receipt.unlink(missing_ok=True)
    commands = rust_commands(partition) if lane == "rust-tests" else runtime_commands(partition)
    seconds = execute(commands)
    write_receipt(lane, partition, seconds)


if __name__ == "__main__":
    try:
        main()
    except (ValueError, KeyError, OSError, subprocess.SubprocessError) as exc:
        print(f"partition failed: {exc}", file=sys.stderr)
        sys.exit(1)
