"""Independent RED acceptance for critical-path partitions (baseline 38e84d2).

Small execution interface for the two formerly monolithic test lanes:

* scripts/ci/run-test-partition.py --lane rust-tests|ignored-db-runtime --list
  prints {"include": [{"partition": "nonempty-id", ...}, ...]} JSON. IDs are
  unique; each lane has at least two nonempty partitions. The scheduling and
  target/selector discovery algorithms are intentionally unspecified.
* The same CLI with --partition ID executes exactly that partition. Missing,
  empty, unknown or repeated selections fail before executing tests.
* A timeboxed command keeps one total deadline across its inventory
  (--list compile/probe) and execution: time spent probing reduces the
  execution allowance, an exhausted deadline or a subprocess timeout is
  fatal with no success receipt, and non-timeboxed commands stay unbounded.
* Worker steps have id: run_partition and literal env CI_TEST_LANE plus env
  CI_TEST_PARTITION: ${{ matrix.partition }} (job env is also accepted). Their
  real run: shell invokes the CLI. Matrices may be literal or fromJSON of a
  needs.<planner>.outputs.<matrix> produced by an executable planner step.
  Stable rust tests / ignored db (runtime) jobs aggregate those workers.

Coverage keeps its existing FULL_LEGS/plan()/SHARD_LEGS interface. We execute
its real YAML shell one leg at a time, not a reimplementation of selected().
The frozen test fixture is a hand-inspected recording of baseline YAML commands
and environments, evaluated against a synthetic inventory including duplicate
names across packages, ignored/nonignored siblings and an unlisted new target.
Counters preserve intentional baseline replays, not just a union of test names.
No real compiler, database, network request, timing or LCOV-parity claim occurs.
"""

import argparse
from copy import deepcopy
import importlib.util
import json
import os
import re
import shutil
import shlex
import subprocess
import sys
import tempfile
import unittest
from collections import Counter
from contextlib import ExitStack
from pathlib import Path
from unittest.mock import patch

from test_critical_path_support import (
    ROOT, Sandbox, assertions_message, ledger, metadata, resolved_env, run_coverage,
    selections, workflow,
)

BASELINE = json.loads(Path(__file__).with_name("test_critical_path_baseline.json").read_text())
ROWS = BASELINE["inventory"]
RUNNER = ROOT / "scripts/ci/run-test-partition.py"


def evolving_inventory():
    # These targets were not present when the golden commands were recorded.
    # A closed hard-coded list must not quietly lose their selected tests.
    extra = [dict(package=package, kind="test", target="late_discovered_contract",
                  name=name, ignored=ignored)
             for package in ("two-bot-core", "two-bot")
             for name, ignored in (("onboarding_late_contract", False),
                                   ("onboarding_late_contract_ignored", True))]
    # The remainder must discover future workspace members too, not just
    # targets in the original hard-coded package families.
    extra += [dict(package="two-bot-future", kind="test", target="new_member_contract",
                   name=name, ignored=ignored)
              for name, ignored in (("property_smoke", False), ("ordinary_ignored", True))]
    return ROWS + extra


def planner():
    spec = importlib.util.spec_from_file_location("critical_path_coverage_planner",
                                                 ROOT / "scripts/ci/plan-coverage-shards.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def load_runner():
    spec = importlib.util.spec_from_file_location(
        "critical_path_partition_runner", RUNNER)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def require_runner(case):
    case.assertTrue(RUNNER.is_file(),
                    "rust-tests/runtime-DB have no executable partition interface; "
                    "add the documented runner, not just more coverage matrix entries")


def list_partitions(case, sandbox, lane):
    require_runner(case)
    proc = sandbox.run(shlex.join([sys.executable, str(RUNNER), "--lane", lane, "--list"]))
    case.assertEqual(proc.returncode, 0, assertions_message(proc))
    value = json.loads(proc.stdout)
    case.assertIsInstance(value.get("include"), list)
    entries = value["include"]
    ids = [row["partition"] for row in entries]
    case.assertTrue(all(isinstance(i, str) and i.strip() == i and i for i in ids))
    case.assertEqual(len(ids), len(set(ids)), "duplicate partition ownership")
    case.assertGreaterEqual(len(ids), 2, f"{lane} is still one indivisible job")
    return entries


def matrix_entries(case, sandbox, job, jobs):
    matrix = job.get("strategy", {}).get("matrix")
    if isinstance(matrix, dict):
        return matrix["include"]
    match = re.fullmatch(r"\$\{\{\s*fromJSON\(needs\.([\w-]+)\.outputs\.([\w-]+)\)\s*\}\}",
                         str(matrix))
    case.assertIsNotNone(match, "partition matrix must be executable or literal, not an opaque test hook")
    producer = jobs[match[1]]
    output = producer["outputs"][match[2]]
    source = re.fullmatch(r"\$\{\{\s*steps\.([\w-]+)\.outputs\.([\w-]+)\s*\}\}", output)
    case.assertIsNotNone(source)
    step = next(s for s in producer["steps"] if s.get("id") == source[1])
    proc = sandbox.run(step["run"], resolved_env(producer, step, sandbox))
    case.assertEqual(proc.returncode, 0, assertions_message(proc))
    outputs = dict(line.split("=", 1) for line in (sandbox.root / "github-output").read_text().splitlines())
    return json.loads(outputs[source[2]])["include"]


def workers(lane):
    found = []
    for name, job in workflow().items():
        for step in job.get("steps", []):
            env = job.get("env", {}) | step.get("env", {})
            if step.get("id") == "run_partition" and env.get("CI_TEST_LANE") == lane:
                found.append((name, job, step))
    return found


def check_disposable_guards(case, sandbox, job, lane):
    prepare = next(s for s in job["steps"] if s.get("name") == "Prepare isolated test databases")
    verify = next(s for s in job["steps"] if s.get("name") == "Verify disposable Postgres defaults")
    settings = dict(data_directory="/var/lib/postgresql/18/docker", fsync="on",
                    full_page_writes="on", synchronous_commit="on",
                    max_wal_size="128MB" if lane == "rust-tests" else "512MB",
                    min_wal_size="32MB" if lane == "rust-tests" else "64MB")
    sandbox.executable("psql", "import os\nprint(os.environ['DB_PROBE_JSON'])\nraise SystemExit(int(os.environ.get('DB_EXIT', '0')))\n")
    sandbox.executable("createdb", "import os\nraise SystemExit(int(os.environ.get('DB_EXIT', '0')))\n")
    normal = {"DB_PROBE_JSON": json.dumps(settings)}
    for step in (prepare, verify):
        env = resolved_env(job, step, sandbox)
        case.assertEqual(sandbox.run(step["run"], env | normal).returncode, 0)
        case.assertNotEqual(sandbox.run(step["run"], env | normal | {"DB_EXIT": "43"}).returncode, 0)
    for key in ("fsync", "full_page_writes", "synchronous_commit"):
        bad = {"DB_PROBE_JSON": json.dumps(settings | {key: "off"})}
        case.assertNotEqual(sandbox.run(verify["run"], resolved_env(job, verify, sandbox) | bad).returncode, 0)
    case.assertNotEqual(sandbox.run(verify["run"], normal | {"PGOPTIONS": "-c fsync=off"}).returncode, 0)


class RecorderControls(unittest.TestCase):
    def test_unrelated_packages_do_not_inherit_a_literal_dependency_feature(self):
        units = selections(["test", "--workspace", "--features", "two-bot-core/db"], ROWS)
        unrelated = [unit for unit in units if unit[0] in ("two-bot-next", "two-bot-testsupport")]
        self.assertTrue(unrelated)
        self.assertTrue(all(unit[4] == () for unit in unrelated),
                        "a package without a core dependency cannot observe core/db")

    def test_a_dependency_feature_is_not_the_consumers_own_feature(self):
        units = selections(["test", "-p", "two-bot-discord", "--features", "two-bot-core/db"], ROWS)
        self.assertTrue(units)
        self.assertEqual(units[0][4], ("two-bot-core/db", "two-bot-core/default",
                                     "two-bot-discord/default", "two-bot-discord/test-support"))
        own = selections(["test", "-p", "two-bot-discord", "--features", "db"], ROWS)
        self.assertIn("two-bot-discord/db", own[0][4])
        self.assertIn("two-bot-core/db", own[0][4])

    def test_selected_workspace_features_unify_before_projection_to_each_test(self):
        broad = selections(["test", "--workspace", "--features", "two-bot-core/db"], ROWS)
        narrow = selections(["test", "-p", "two-bot-discord", "--features", "two-bot-core/db"], ROWS)
        discord = next(unit for unit in broad if unit[0] == "two-bot-discord")
        self.assertIn("two-bot-discord/db", discord[4], "bot enables its dependency's db feature")
        self.assertNotIn("two-bot-discord/db", narrow[0][4])
        self.assertNotEqual(discord[4], narrow[0][4], "raw CLI flags hide lost feature unification")

    def test_dependency_feature_requires_a_selected_owner_or_direct_dependency(self):
        for args in (["-p", "two-bot-testsupport"],
                     ["--workspace", "--exclude", "two-bot-core", "--exclude", "two-bot",
                      "--exclude", "two-bot-discord", "--exclude", "two-bot-cutover",
                      "--exclude", "two-bot-store"]):
            with self.subTest(args=args), self.assertRaises(ValueError):
                selections(["test", *args, "--features", "two-bot-core/db"], ROWS)

    def test_feature_disabled_targets_are_not_implicit_test_selections(self):
        row = dict(package="two-bot-core", kind="test", target="requires_db",
                   name="feature_gated", ignored=False, required_features=["db"])
        self.assertEqual(selections(["test", "-p", "two-bot-core"], [row]), [])
        enabled = selections(["test", "-p", "two-bot-core", "--features", "db"], [row])
        self.assertEqual([unit[:4] for unit in enabled],
                         [("two-bot-core", "test", "requires_db", "feature_gated")])

    def test_package_target_and_ignored_filters_are_independent(self):
        rows = [dict(package=p, kind="test", target="same_name", name=n, ignored=i)
                for p in ("two-bot-core", "two-bot-discord")
                for n, i in (("wanted", False), ("wanted_ignored", True), ("other", False))]
        got = selections(shlex.split("test -p two-bot-discord --test same_name --locked wanted -- --ignored"), rows)
        self.assertEqual([r[:4] for r in got],
                         [("two-bot-discord", "test", "same_name", "wanted_ignored")])
        self.assertEqual(selections(shlex.split("test --workspace --tests -- --ignored --skip wanted"), rows), [])

    def test_baseline_disposable_prepare_and_verify_fail_closed(self):
        sandbox = Sandbox(ROWS)
        self.addCleanup(sandbox.close)
        for lane in ("rust-tests", "ignored-db-runtime"):
            with self.subTest(lane=lane):
                found = workers(lane)
                job = found[0][1] if found else workflow()[lane]
                check_disposable_guards(self, sandbox, job, lane)

    def test_frozen_controls_include_new_targets_and_intentional_replays(self):
        rust = ledger(BASELINE["lanes"]["rust-tests"], ROWS)
        self.assertTrue(any(key[2] == "future_unlisted_contract" for key in rust))
        ordinary = ("two-bot-core", "lib", "two_bot_core", "property_smoke",
                    ("two-bot-core/db", "two-bot-core/default"), None, False)
        self.assertEqual(rust[ordinary], 2, "ordinary and property-budget replay must both count")
        self.assertGreater(sum(ledger(BASELINE["lanes"]["coverage"], ROWS).values()), 100)


class CoveragePartitionAcceptance(unittest.TestCase):
    def setUp(self):
        self.rows = evolving_inventory()
        self.sandbox = Sandbox(self.rows)
        self.addCleanup(self.sandbox.close)
        self.planner = planner()

    def executions(self):
        by_leg = {}
        for leg in self.planner.FULL_LEGS:
            self.sandbox.clear()
            proc = run_coverage(self.sandbox, [leg["id"]])
            self.assertEqual(proc.returncode, 0, leg["id"] + "\n" + assertions_message(proc))
            by_leg[leg["id"]] = self.sandbox.records()
        return by_leg

    def test_each_broad_coverage_sweep_has_multiple_nonempty_disjoint_owners(self):
        executions = self.executions()
        for ignored in (False, True):
            with self.subTest(ignored=ignored):
                owners = {}
                for leg, records in executions.items():
                    primary = [r for r in records if "--ignore-run-fail" in r["argv"]
                               and ("--ignored" in r["argv"]) == ignored]
                    units = ledger(primary, self.rows)
                    if units:
                        owners[leg] = units
                self.assertGreaterEqual(len(owners), 2,
                                        f"{'ignored' if ignored else 'nonignored'} workspace sweep is indivisible")
                baseline = [r for r in BASELINE["lanes"]["coverage"]
                            if "--ignore-run-fail" in r["argv"]
                            and ("--ignored" in r["argv"]) == ignored]
                want = ledger(baseline, self.rows)
                got = sum(owners.values(), Counter())
                self.assertEqual(got, want)
                self.assertTrue(all(n == 1 for n in got.values()), "accidental duplicate sweep ownership")
                self.assertTrue(all(units != want for units in owners.values()))

    def test_full_execution_preserves_features_filters_replays_and_effective_environment(self):
        got = [r for records in self.executions().values() for r in records]
        self.assertEqual(ledger(got, self.rows, environments=True),
                         ledger(BASELINE["lanes"]["coverage"], self.rows, environments=True))

    def test_ops_and_starboard_replays_have_one_appropriate_sweep_partition_owner(self):
        executions = self.executions()
        for needle, ignored in (("settings_jobs::tests::ops_overrides_acceptance_tests::", False),
                                ("starboard_store", True)):
            with self.subTest(replay=needle):
                owners = {leg: records for leg, records in executions.items()
                          if any(needle in r["argv"] and "--ignore-run-fail" not in r["argv"] for r in records)}
                self.assertEqual(len(owners), 1, "isolated replay must run once, not on every shard")
                records = next(iter(owners.values()))
                self.assertTrue(any("--ignore-run-fail" in r["argv"]
                                    and ("--ignored" in r["argv"]) == ignored for r in records),
                                "isolated replay lost its sweep-family owner")

    def test_scoped_expanded_plans_keep_reverse_dependents_and_widen_when_metadata_is_incomplete(self):
        executions = self.executions()
        graph = metadata(self.rows)
        for node in graph["resolve"]["nodes"]:
            if node["id"] == "two-bot-discord":
                node["deps"] = [{"name": "two-bot-core", "pkg": "two-bot-core", "dep_kinds": [{"kind": "dev", "target": None}]}]
            elif node["id"] == "two-bot":
                node["deps"] = [{"name": "two-bot-discord", "pkg": "two-bot-discord", "dep_kinds": [{"kind": None, "target": None}]}]
        value = self.planner.plan(["crates/core/src/lib.rs"], 8,
                                  event_name="pull_request", ref="refs/pull/1/merge", metadata=graph)
        self.assertFalse(value["widened"], "complete metadata must permit scoped selection")
        records = [r for leg in value["legs"] for r in executions[leg]]
        got = ledger(records, self.rows)
        want = ledger(BASELINE["lanes"]["coverage"], self.rows)
        affected = {"two-bot-core", "two-bot-discord", "two-bot"}
        self.assertFalse(any(got[key] < count for key, count in want.items() if key[0] in affected),
                         "a changed crate or transitive reverse dependent lost a selected execution")
        incomplete = metadata(self.rows)
        incomplete["resolve"]["nodes"].pop()
        full_ids = {leg["id"] for leg in self.planner.FULL_LEGS}
        for paths, event, ref, graph in (
            (["crates/core/src/lib.rs"], "pull_request", "refs/pull/1/merge", incomplete),
            (["unknown/input.rs"], "pull_request", "refs/pull/1/merge", None),
            (["Cargo.toml"], "pull_request", "refs/pull/1/merge", None),
            (["crates/core/src/lib.rs"], "schedule", "refs/heads/main", None),
            (["crates/core/src/lib.rs"], "push", "refs/heads/main", None),
        ):
            with self.subTest(paths=paths, event=event):
                value = self.planner.plan(paths, 8, event_name=event, ref=ref, metadata=graph)
                self.assertTrue(value["widened"])
                self.assertEqual(set(value["legs"]), full_ids)
                flat = [leg for shard in value["shards"] for leg in shard]
                self.assertCountEqual(flat, full_ids)
                self.assertEqual(len(flat), len(set(flat)))

    def test_empty_duplicate_and_unknown_leg_selections_fail_before_measurement(self):
        valid = self.planner.FULL_LEGS[0]["id"]
        for ids in ([], [valid, valid], ["not-a-coverage-leg"]):
            with self.subTest(ids=ids):
                self.sandbox.clear()
                proc = run_coverage(self.sandbox, ids)
                self.assertNotEqual(proc.returncode, 0, "invalid selection passed: " + repr(ids))
                self.assertEqual(self.sandbox.records(), [], "invalid selection executed partial work")

    def test_build_and_guarded_replay_failures_cannot_export_success(self):
        for leg in self.planner.FULL_LEGS:
            self.sandbox.clear()
            proc = run_coverage(self.sandbox, [leg["id"]], FAIL_NEEDLE="--locked")
            self.assertNotEqual(proc.returncode, 0, leg["id"] + " tolerated compilation/replay failure")
            self.assertFalse((self.sandbox.root / "lcov-shard-0.info").exists())

    def test_guarded_channel_replay_unsets_inherited_libpq_credentials_and_options(self):
        candidates = [leg["id"] for leg in self.planner.FULL_LEGS
                      if "--test channel_moderation_runtime" in leg["command"]]
        self.assertTrue(candidates)
        for leg_id in candidates:
            self.sandbox.clear()
            proc = run_coverage(self.sandbox, [leg_id], PGPASSWORD="fixture-only",
                                PGPASSFILE="/fixture/not-a-real-passfile", PGOPTIONS="-c fsync=off")
            self.assertEqual(proc.returncode, 0, assertions_message(proc))
            guarded = [r for r in self.sandbox.records() if "channel_moderation_runtime" in r["argv"]]
            self.assertTrue(guarded)
            for record in guarded:
                self.assertTrue(all(key not in record["env"] for key in ("PGPASSWORD", "PGPASSFILE", "PGOPTIONS")))

    def test_empty_report_and_failed_report_remain_fatal(self):
        leg = self.planner.FULL_LEGS[0]["id"]
        for overrides in ({"EMPTY_REPORT": "1"}, {"REPORT_EXIT": "47"}):
            with self.subTest(overrides=overrides):
                proc = run_coverage(self.sandbox, [leg], **overrides)
                self.assertNotEqual(proc.returncode, 0, assertions_message(proc))


class TestLanePartitionAcceptance(unittest.TestCase):
    def setUp(self):
        self.rows = evolving_inventory()
        self.sandbox = Sandbox(self.rows)
        self.addCleanup(self.sandbox.close)

    def test_rust_tests_are_exhaustive_disjoint_and_wired_to_the_real_workflow(self):
        self.check_lane("rust-tests")

    def test_runtime_db_is_exhaustive_disjoint_and_wired_to_the_real_workflow(self):
        self.check_lane("ignored-db-runtime")

    def check_lane(self, lane):
        entries = list_partitions(self, self.sandbox, lane)
        found = workers(lane)
        self.assertEqual(len(found), 1, f"{lane} needs one matrix worker, not an unused runner")
        _, job, step = found[0]
        self.assertEqual(job["strategy"].get("fail-fast"), False)
        self.assertNotIn("continue-on-error", job)
        self.assertFalse(step.get("continue-on-error", False))
        self.assertEqual(matrix_entries(self, self.sandbox, job, workflow()), entries)
        self.assertIn("agent-testdb", job.get("services", {}))
        names = [s.get("name", "") for s in job.get("steps", [])]
        self.assertIn("Prepare isolated test databases", names)
        self.assertIn("Verify disposable Postgres defaults", names)
        self.assertLess(names.index("Prepare isolated test databases"), names.index(step["name"]))
        self.assertLess(names.index("Verify disposable Postgres defaults"), names.index(step["name"]))
        check_disposable_guards(self, self.sandbox, job, lane)
        condition = str(step.get("if", "success()")).removeprefix("${{").removesuffix("}}").strip()
        if "always()" in condition:
            self.assertIn("!cancelled()", condition)
            for name in ("Prepare isolated test databases", "Verify disposable Postgres defaults"):
                guard = next(s for s in job["steps"] if s.get("name") == name)
                self.assertTrue(guard.get("id"), "always-run partitions need explicit successful DB guards")
                self.assertIn("steps." + guard["id"] + ".outcome == 'success'", condition)
        else:
            self.assertEqual(condition, "success()", "test execution must be suppressed after setup failure")
        per_partition = []
        for row in entries:
            self.sandbox.clear()
            env = resolved_env(job, step, self.sandbox, row)
            self.assertEqual(env["CI_TEST_PARTITION"], row["partition"])
            proc = self.sandbox.run(step["run"], env)
            self.assertEqual(proc.returncode, 0, assertions_message(proc))
            records = self.sandbox.records()
            self.assertTrue(ledger(records, self.rows), "empty test partition")
            per_partition.append(records)
        got = ledger([r for part in per_partition for r in part], self.rows, environments=True)
        want = ledger(BASELINE["lanes"][lane], self.rows, environments=True)
        self.assertEqual(got, want, "lost, repeated, feature-drifted or environment-drifted baseline executions")
        self.assertTrue(all(ledger(part, self.rows, environments=True) != want for part in per_partition))

    def test_invalid_lane_partition_and_missing_or_repeated_selection_are_fatal(self):
        require_runner(self)
        for lane in ("rust-tests", "ignored-db-runtime"):
            entries = list_partitions(self, self.sandbox, lane)
            base = [sys.executable, str(RUNNER), "--lane", lane]
            cases = ([], ["--partition", ""], ["--partition", "unknown-partition"],
                     ["--partition", entries[0]["partition"], "--partition", entries[0]["partition"]])
            for args in cases:
                with self.subTest(lane=lane, args=args):
                    self.sandbox.clear()
                    proc = self.sandbox.run(shlex.join(base + args))
                    self.assertNotEqual(proc.returncode, 0, assertions_message(proc))
                    self.assertEqual(self.sandbox.records(), [])
        proc = self.sandbox.run(shlex.join([sys.executable, str(RUNNER), "--lane", "unknown", "--list"]))
        self.assertNotEqual(proc.returncode, 0)

    def test_empty_selected_inventory_cannot_succeed_as_a_measured_partition(self):
        require_runner(self)
        empty = Sandbox([])
        self.addCleanup(empty.close)
        for lane in ("rust-tests", "ignored-db-runtime"):
            entries = list_partitions(self, self.sandbox, lane)
            for row in entries:
                with self.subTest(lane=lane, partition=row["partition"]):
                    empty.clear()
                    proc = empty.run(shlex.join([
                        sys.executable, str(RUNNER), "--lane", lane, "--partition", row["partition"]]))
                    self.assertNotEqual(proc.returncode, 0, "zero selected tests were reported as a completed partition")

    def test_each_test_partition_propagates_cargo_failure(self):
        for lane in ("rust-tests", "ignored-db-runtime"):
            entries = list_partitions(self, self.sandbox, lane)
            for row in entries:
                with self.subTest(lane=lane, partition=row["partition"]):
                    proc = self.sandbox.run(shlex.join([
                        sys.executable, str(RUNNER), "--lane", lane, "--partition", row["partition"]]),
                        {"FAIL_NEEDLE": "--locked"})
                    self.assertNotEqual(proc.returncode, 0, "partition swallowed a failed test")

    def test_integration_partitions_prepare_effective_scratch_root_before_cargo(self):
        # A scratch-child test (backup_response_redaction.rs:25) creates a
        # child of PAPERCLIP_RUN_SCRATCH_DIR with fs::create_dir. When the
        # runner rewrites that root for integration commands it must create
        # the effective root before Cargo inventory/execution; relying on
        # some sibling target or partition to have created it made a fresh
        # integration partition fail with ENOENT. This isolates the rewrite
        # case: the fixture deletes every pre-created root first.
        shutil.rmtree(self.sandbox.root / "two-bot-check-scratch")
        rewritten = set()
        for index in range(3):
            partition = f"integration-{index}"
            with self.subTest(partition=partition):
                self.sandbox.clear()
                proc = self.sandbox.run(shlex.join([
                    sys.executable, str(RUNNER), "--lane", "rust-tests",
                    "--partition", partition]))
                self.assertEqual(proc.returncode, 0, assertions_message(proc))
                histories = [
                    record for record in self.sandbox.records()
                    if record.get("effective_paperclip_run_scratch_dir") is not None
                    and record.get("env", {}).get("PAPERCLIP_RUN_SCRATCH_DIR")
                        != "$RUNNER_TEMP/two-bot-check-scratch"
                ]
                self.assertTrue(histories, "no integration command rewrote the scratch root")
                for record in histories:
                    effective = Path(record["effective_paperclip_run_scratch_dir"])
                    rewritten.add(effective)
                    self.assertTrue(
                        effective.is_dir(),
                        f"integration command ran with unprepared scratch root {effective}",
                    )
                    child = effective / "s3-response-redaction-fixture"
                    child.mkdir(exist_ok=True)
                    self.assertTrue(child.is_dir())
        self.assertEqual(len(rewritten), 1, "the integration scratch rewrite drifted")

    def test_scratch_preparation_failure_is_fatal_before_cargo_without_receipt(self):
        # Same contract from the failure side: if preparing the effective
        # scratch root fails (here a regular file blocks its path), the
        # runner must fail closed before any Cargo execution and write no
        # success receipt.
        blocker = self.sandbox.root / "two-bot-backup-tests"
        blocker.write_text("not a directory\n")
        for index in range(3):
            partition = f"integration-{index}"
            with self.subTest(partition=partition):
                self.sandbox.clear()
                proc = self.sandbox.run(shlex.join([
                    sys.executable, str(RUNNER), "--lane", "rust-tests",
                    "--partition", partition]))
                self.assertNotEqual(
                    proc.returncode, 0,
                    "scratch preparation failure was tolerated: " + assertions_message(proc),
                )
                self.assertEqual(
                    self.sandbox.records(), [],
                    "Cargo executed despite the unpreparable scratch root",
                )
                receipt = (self.sandbox.root / "test-partition-receipts"
                           / f"rust-tests-{partition}.json")
                self.assertFalse(receipt.exists(), "preparation failure wrote a receipt")


class _FakeClock:
    def __init__(self):
        self.now = 10_000.0
        self.wall = 1_700_000_000.0

    def advance(self, seconds, wall_jump=0.0):
        self.now += seconds
        self.wall += seconds + wall_jump

    def monotonic(self):
        return self.now

    def monotonic_ns(self):
        return int(self.now * 1_000_000_000)

    def time(self):
        return self.wall


class _RecordedProcess:
    """Deterministic process boundary for either run() or Popen() callers."""

    def __init__(self, recorder, argv, kwargs, pid):
        self.recorder, self.args, self.kwargs = recorder, list(argv), kwargs
        self.pid, self.returncode, self.output = pid, None, None

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.wait()

    def communicate(self, input=None, timeout=None):
        if self.returncode is None:
            result = self.recorder.run(self.args, **(self.kwargs | {"timeout": timeout}))
            self.returncode, self.output = result.returncode, result.stdout
        return self.output, None

    def poll(self):
        return self.returncode

    def wait(self, timeout=None):
        if self.returncode is None:
            self.communicate(timeout=timeout)
        return self.returncode

    def send_signal(self, sig):
        self.returncode = -sig

    def kill(self):
        self.send_signal(9)

    def terminate(self):
        self.send_signal(15)


class _SubprocessRecorder:
    """Consumes fake durations under the actual timeout supplied by the runner."""

    def __init__(self, *, inventory_advance=0.0, inventory_timeout=False,
                 execution_timeout=False, wall_jump=0.0, execution_advance=0.0):
        self.clock = _FakeClock()
        self.calls = []
        self._inventory_advance = inventory_advance
        self._execution_advance = execution_advance
        self._wall_jump = wall_jump
        self._advanced = False
        self._inventory_timeout = inventory_timeout
        self._execution_timeout = execution_timeout
        self.processes = []

    def popen(self, argv, **kwargs):
        process = _RecordedProcess(self, argv, kwargs, 900_000_000 + len(self.processes))
        self.processes.append(process)
        return process

    def process(self, pid):
        return next(process for process in self.processes if process.pid == pid)

    def signal_group(self, pid, sig):
        # Never send a host signal for a synthetic PID. Real process-tree
        # termination is independently tested with finite native children.
        self.process(pid).send_signal(sig)

    def group_id(self, pid):
        return self.process(pid).pid

    def run(self, argv, **kwargs):
        argv = list(argv)
        timeout = kwargs.get("timeout")
        self.calls.append({
            "argv": argv,
            "timeout": timeout,
            "env": kwargs.get("env") or {},
        })
        duration = 0.0
        if "--list" in argv:
            if not self._advanced:
                duration = self._inventory_advance
                self.clock.advance(duration, self._wall_jump)
                self._advanced = True
            forced_timeout = self._inventory_timeout
        else:
            if "alive" in argv:
                duration = self._execution_advance
                self.clock.advance(duration)
            forced_timeout = self._execution_timeout and "alive" in argv
        if forced_timeout or (timeout is not None and duration > timeout):
            raise subprocess.TimeoutExpired(argv, timeout)
        return subprocess.CompletedProcess(
            argv, 0, stdout="alive_fixture: test\n" if "--list" in argv else "")

    def inventory_calls(self):
        return [call for call in self.calls if "--list" in call["argv"]]

    def execution_calls(self):
        return [call for call in self.calls if "--list" not in call["argv"]]


class PartitionRunnerTimeoutTests(unittest.TestCase):
    """The 60-second `alive` bound covers inventory and execution together.

    The old lane bound the whole compile/list/execute sequence; probing
    the selected tests with `--list` compiles the same binary, so a
    timeout that only starts at execution lets the probe hang unbounded
    and then grants a second full budget. Deterministic only: a fake
    clock and a subprocess recorder, never a real sleep or Cargo run.
    """

    def setUp(self):
        self.runner = load_runner()

    def patched(self, recorder):
        clock = recorder.clock
        return (
            patch.object(self.runner.subprocess, "Popen", recorder.popen),
            patch.object(self.runner.os, "killpg", recorder.signal_group),
            patch.object(self.runner.os, "getpgid", recorder.group_id),
            patch.object(self.runner.time, "monotonic", clock.monotonic),
            patch.object(self.runner.time, "monotonic_ns", clock.monotonic_ns),
            patch.object(self.runner.time, "perf_counter", clock.monotonic),
            patch.object(self.runner.time, "time", clock.time),
        )

    def execute_other(self, recorder):
        with ExitStack() as patches:
            for patched in self.patched(recorder):
                patches.enter_context(patched)
            return self.runner.execute(self.runner.runtime_commands("other"))

    def run_main_other(self, recorder, runner_temp):
        receipt = (Path(runner_temp) / "test-partition-receipts"
                   / "ignored-db-runtime-other.json")
        receipt.parent.mkdir(parents=True, exist_ok=True)
        receipt.write_text("stale receipt from an earlier attempt\n")
        argv = ["run-test-partition.py", "--lane", "ignored-db-runtime",
                "--partition", "other"]
        patches = self.patched(recorder) + (
            patch.object(sys, "argv", argv),
            patch.dict(os.environ, {"RUNNER_TEMP": str(runner_temp)}),
        )
        failure = None
        with ExitStack() as stack:
            for patched in patches:
                stack.enter_context(patched)
            try:
                self.runner.main()
            except BaseException as exc:  # SystemExit is also a fatal outcome
                failure = exc
        return failure, receipt

    def assert_fatal(self, failure):
        self.assertIsNotNone(failure, "timeout path reported success")
        if isinstance(failure, SystemExit):
            self.assertNotEqual(failure.code, 0)

    def assert_alive_remainder(self, elapsed, wall_jump, expected):
        recorder = _SubprocessRecorder(inventory_advance=elapsed, wall_jump=wall_jump)
        try:
            seconds = self.execute_other(recorder)
        except subprocess.TimeoutExpired as exc:
            self.fail(f"wall-clock adjustment exhausted a positive monotonic remainder: {exc}")
        self.assertEqual(seconds, 0.0)
        inventory = [call for call in recorder.inventory_calls()
                     if "alive" in call["argv"]]
        self.assertTrue(inventory, "alive was never probed before execution")
        probe = inventory[0]
        self.assertEqual(
            probe["argv"],
            ["cargo", "test", "-p", "two-bot", "--locked", "--test", "alive",
             "--", "--ignored", "--nocapture", "--list"])
        self.assertIsNotNone(
            probe["timeout"], "alive inventory probe ran without a timeout")
        self.assertGreater(probe["timeout"], 0)
        self.assertLessEqual(probe["timeout"], 60)
        self.assertAlmostEqual(probe["timeout"], 60.0, delta=0.000001)
        executions = recorder.execution_calls()
        self.assertEqual(len(executions), 5)
        alive = executions[0]
        self.assertEqual(
            alive["argv"],
            ["cargo", "test", "-p", "two-bot", "--locked", "--test", "alive",
             "--", "--ignored", "--nocapture"])
        self.assertIsNotNone(alive["timeout"])
        self.assertAlmostEqual(
            alive["timeout"], expected, delta=0.000001,
            msg="execution did not receive the positive remainder of the one 60-second deadline")
        # Non-timeboxed commands keep their unbounded shape and routing.
        self.assertTrue(all(call["timeout"] is None
                            for call in executions[1:]))
        self.assertNotIn("TWO_TEST_DATABASE_URL", alive["env"])
        self.assertTrue(
            executions[1]["env"]["TWO_TEST_DATABASE_URL"].endswith(
                "/two_bot_test_ci"),
            "the restore_drill replay lost its disposable database")

    def test_alive_inventory_and_execution_share_one_deadline(self):
        # Independent wall jumps must neither enlarge nor consume elapsed-time
        # budgets. Literal remainders catch a fixed 35-second minimum as well.
        for elapsed, expected in ((0.0, 60.0), (25.0, 35.0),
                                  (50.0, 10.0), (59.5, 0.5)):
            for wall_jump in (-3600.0, 0.0, 3600.0):
                with self.subTest(elapsed=elapsed, wall_jump=wall_jump):
                    self.assert_alive_remainder(elapsed, wall_jump, expected)

    def test_exhausted_alive_deadline_prevents_execution_and_receipt(self):
        for elapsed in (60.0, 61.0):
            for wall_jump in (-3600.0, 0.0, 3600.0):
                with self.subTest(elapsed=elapsed, wall_jump=wall_jump):
                    recorder = _SubprocessRecorder(
                        inventory_advance=elapsed, wall_jump=wall_jump)
                    with tempfile.TemporaryDirectory() as runner_temp:
                        failure, receipt = self.run_main_other(recorder, runner_temp)
                        self.assertFalse(receipt.exists(),
                                         "exhausted deadline left a success receipt")
                    self.assert_fatal(failure)
                    self.assertTrue(recorder.inventory_calls())
                    self.assertEqual(
                        recorder.execution_calls(), [],
                        "execution started after the total deadline was already spent")

    def test_execution_cannot_outlive_a_small_positive_remainder(self):
        for wall_jump in (-3600.0, 0.0, 3600.0):
            with self.subTest(wall_jump=wall_jump):
                recorder = _SubprocessRecorder(
                    inventory_advance=50.0, execution_advance=11.0, wall_jump=wall_jump)
                with tempfile.TemporaryDirectory() as runner_temp:
                    failure, receipt = self.run_main_other(recorder, runner_temp)
                    self.assertFalse(receipt.exists(),
                                     "11-second execution outlived its 10-second remainder")
                self.assert_fatal(failure)
                self.assertIsInstance(failure, subprocess.TimeoutExpired)
                executions = recorder.execution_calls()
                self.assertEqual(len(executions), 1, "expired execution did not stop the lane")
                self.assertIn("alive", executions[0]["argv"])
                self.assertAlmostEqual(executions[0]["timeout"], 10.0, delta=0.000001)

    def test_inventory_timeout_is_fatal_without_execution_or_receipt(self):
        recorder = _SubprocessRecorder(inventory_timeout=True)
        with tempfile.TemporaryDirectory() as runner_temp:
            failure, receipt = self.run_main_other(recorder, runner_temp)
            self.assertFalse(receipt.exists(),
                             "inventory timeout left a success receipt")
        self.assert_fatal(failure)
        self.assertEqual(recorder.execution_calls(), [])

    def test_execution_timeout_is_fatal_without_receipt_and_stops_lane(self):
        recorder = _SubprocessRecorder(execution_timeout=True)
        with tempfile.TemporaryDirectory() as runner_temp:
            failure, receipt = self.run_main_other(recorder, runner_temp)
            self.assertFalse(receipt.exists(),
                             "execution timeout left a success receipt")
        self.assert_fatal(failure)
        executions = recorder.execution_calls()
        self.assertEqual(len(executions), 1,
                         "lane continued after the timed-out command")
        self.assertIn("alive", executions[0]["argv"])


class RuntimeSmokePreparationAcceptance(unittest.TestCase):
    """Cold-worker compile setup must not consume the alive execution deadline.

    Execute the parsed generator/preparation/partition shells with the real
    partition runner and synthetic Cargo. Only a successful compile-only call
    warms the cold fixture. The narrow Actions model handles default success,
    status functions and matrix equality; unknown expressions fail the fixture.
    Artifact upload eligibility is modeled, not the native artifact action.
    Host/container/DB setup before the generator is outside this harness.
    """

    GENERATOR = "Generate command reference before runtime test compilation"
    UPLOAD = "Upload successful test partition receipt"

    def step_runs(self, step, partition, success):
        condition = str(step.get("if", "success()")).strip()
        condition = condition.removeprefix("${{").removesuffix("}}").strip()
        status = {"success()": success, "failure()": not success,
                  "always()": True, "cancelled()": False, "!cancelled()": True}
        terms = [term.strip() for term in condition.split("&&")]
        allowed = success if not any(term in status for term in terms) else True
        for term in terms:
            matrix = re.fullmatch(r"matrix\.partition\s*(==|!=)\s*'([^']+)'", term)
            if matrix:
                match = partition == matrix[2]
                allowed &= match if matrix[1] == "==" else not match
            elif term in status:
                allowed &= status[term]
            elif term in ("true", "false", "True", "False"):
                allowed &= term.lower() == "true"
            else:
                self.fail(f"unmodeled Actions condition: {condition}")
        return allowed

    def exercise(self, job, partition, *, prep_exit=0, generator_exit=0,
                 prior_success=True):
        sandbox = Sandbox(ROWS)
        self.addCleanup(sandbox.close)
        scripts = sandbox.root / "scripts/ci"
        scripts.mkdir(parents=True)
        for name in ("run-test-partition.py", "cargo_feature_selection.py"):
            shutil.copyfile(ROOT / "scripts/ci" / name, scripts / name)
        trace = sandbox.root / "smoke-events.jsonl"
        generator = sandbox.root / "scripts/generate_command_reference.py"
        generator.write_text(
            "import json, os\n"
            "with open(os.environ['SMOKE_TRACE'], 'a') as out:\n"
            "    out.write(json.dumps({'kind': 'generator'}) + '\\n')\n"
            "raise SystemExit(int(os.environ['GENERATOR_EXIT']))\n")
        cargo = sandbox.bin / "cargo"
        cargo.rename(sandbox.bin / "cargo-execution-fixture")
        sandbox.executable("cargo", '''import json, os, sys
from pathlib import Path
args = sys.argv[1:]
cargo_args = args[:args.index('--')] if '--' in args else args
compile_only = '--no-run' in cargo_args
kind = 'prepare' if compile_only else 'cargo'
profile = {k: v for k, v in os.environ.items()
           if k.startswith('CARGO_') or k in ('RUSTFLAGS', 'RUSTDOCFLAGS', 'RUSTUP_TOOLCHAIN')}
with open(os.environ['SMOKE_TRACE'], 'a') as out:
    out.write(json.dumps({'kind': kind, 'argv': args, 'profile': profile,
                          'step': int(os.environ['SMOKE_STEP'])}) + '\\n')
prepared = Path(os.environ['RUNNER_TEMP']) / 'smoke-prepared'
if compile_only:
    code = int(os.environ['PREP_EXIT'])
    if code == 0:
        prepared.write_text('compiled, not executed\\n')
    raise SystemExit(code)
if '--test' in cargo_args and cargo_args[cargo_args.index('--test') + 1] == 'alive' and not prepared.exists():
    raise SystemExit('cold alive artifact: compile-only preparation never succeeded')
fixture = str(Path(__file__).with_name('cargo-execution-fixture'))
os.execv(fixture, [fixture, *args])
''')
        steps = job["steps"]
        first = next(i for i, step in enumerate(steps)
                     if step.get("name") == self.GENERATOR)
        run_index = next(i for i, step in enumerate(steps)
                         if step.get("id") == "run_partition")
        last = next(i for i, step in enumerate(steps)
                    if step.get("name") == self.UPLOAD)
        self.assertLess(first, run_index)
        self.assertLess(run_index, last)
        receipt = (sandbox.root / "test-partition-receipts"
                   / f"ignored-db-runtime-{partition}.json")
        attempts, uploads, output = [], [], []
        success = prior_success
        shared_env = dict(SMOKE_TRACE=str(trace), PREP_EXIT=str(prep_exit),
                          GENERATOR_EXIT=str(generator_exit))
        for index in range(first, last + 1):
            step = steps[index]
            if not self.step_runs(step, partition, success):
                continue
            if "uses" in step:
                self.assertTrue(step["uses"].startswith("actions/upload-artifact@"))
                uploads.append(index)
                success &= receipt.is_file()
                continue
            attempts.append(index)
            if index == run_index:
                with trace.open("a") as out:
                    out.write(json.dumps(dict(kind="partition", receipt_exists=receipt.exists())) + "\n")
            shell = step.get("shell", "sh")
            self.assertIn(shell, ("sh", "bash"), "unmodeled workflow shell")
            argv = (["sh", "-e", "-c"] if shell == "sh" else
                    ["bash", "--noprofile", "--norc", "-eo", "pipefail", "-c"])
            env = (sandbox.env | shared_env
                   | resolved_env(job, step, sandbox, {"partition": partition})
                   | {"SMOKE_STEP": str(index)})
            result = subprocess.run(
                argv + [step["run"]], cwd=sandbox.root, env=env,
                capture_output=True, text=True, timeout=30)
            output.append(assertions_message(result))
            if result.returncode and not step.get("continue-on-error", False):
                success = False
            github_env = sandbox.root / "github-env"
            if github_env.exists():
                shared_env.update(line.split("=", 1) for line in
                                  github_env.read_text().splitlines())
        events = ([json.loads(line) for line in trace.read_text().splitlines()]
                  if trace.exists() else [])
        return dict(sandbox=sandbox, events=events, attempts=attempts,
                    uploads=uploads, success=success, receipt=receipt,
                    run_index=run_index, output="\n".join(output))

    def compile_identity(self, argv):
        parser = argparse.ArgumentParser(add_help=False)
        parser.add_argument("command")
        for flags, dest in ((("-p", "--package"), "package"),
                            (("--test",), "test"), (("--features", "-F"), "features")):
            parser.add_argument(*flags, dest=dest, action="append", default=[])
        for flag in ("--profile", "--target"):
            parser.add_argument(flag)
        for flag in ("--locked", "--no-run", "--release", "--all-features",
                     "--no-default-features"):
            parser.add_argument(flag, action="store_true")
        value, extra = parser.parse_known_args(argv)
        self.assertEqual(extra, [], "preparation added a test filter or unsupported build option")
        return vars(value)

    def assert_other_success(self, job):
        observed = self.exercise(job, "other")
        events = observed["events"]
        preparations = [event for event in events if event["kind"] == "prepare"]
        self.assertEqual(len(preparations), 1,
                         "cold other worker needs one executed compile-only preparation")
        prep = preparations[0]
        step = job["steps"][prep["step"]]
        self.assertFalse(step.get("continue-on-error", False))
        bound = step.get("timeout-minutes")
        self.assertIsNotNone(bound, "compile preparation has no finite step bound")
        self.assertGreater(float(bound), 0)
        self.assertLessEqual(float(bound), 10, "compile setup outgrew its small finite budget")
        worker_bounds = [int(n) for n in re.findall(r"\b\d+\b", str(job["timeout-minutes"]))]
        self.assertTrue(worker_bounds)
        self.assertLess(float(bound), min(worker_bounds))
        expected = dict(command="test", package=["two-bot"], test=["alive"],
                        features=[], profile=None, target=None, locked=True,
                        no_run=True, release=False, all_features=False,
                        no_default_features=False)
        self.assertEqual(self.compile_identity(prep["argv"]), expected,
                         "compile setup changed alive's package/target/features/profile/lock identity")
        probes = [event for event in events if event["kind"] == "cargo"
                  and "alive" in event["argv"]]
        self.assertEqual(len(probes), 2, "alive must still be inventoried and executed")
        for event in probes:
            live_args = event["argv"][:event["argv"].index("--")]
            self.assertEqual(self.compile_identity(live_args), expected | {"no_run": False})
            self.assertEqual(prep["profile"], event["profile"],
                             "preparation and timed command compiled with different build environments")
        kinds = [event["kind"] for event in events]
        self.assertEqual(kinds[:3], ["generator", "prepare", "partition"])
        partition = next(event for event in events if event["kind"] == "partition")
        self.assertFalse(partition["receipt_exists"], "compile setup wrote an alternative receipt")
        self.assertTrue(observed["success"], observed["output"])
        self.assertEqual(len(observed["uploads"]), 1)
        self.assertTrue(observed["receipt"].is_file())
        return prep["step"], observed

    def assert_prep_failure_is_fatal(self, job):
        observed = self.exercise(job, "other", prep_exit=41)
        self.assertEqual(sum(e["kind"] == "prepare" for e in observed["events"]), 1,
                         "failure control never reached compile-only setup")
        self.assertFalse(observed["success"], "compile error was tolerated")
        self.assertNotIn(observed["run_index"], observed["attempts"],
                         "partition ran after compile preparation failed")
        self.assertEqual(observed["uploads"], [], "compile error made receipt upload eligible")
        self.assertFalse(observed["receipt"].exists())
        self.assertEqual(observed["sandbox"].records(), [],
                         "compile preparation executed tests or continued the partition")

    def assert_contract(self, job):
        prep_index, observed = self.assert_other_success(job)
        records = observed["sandbox"].records()
        for partition in ("onboarding", "gateway", "runtime", "cutover"):
            other = self.exercise(job, partition)
            self.assertFalse(any(e["kind"] == "prepare" for e in other["events"]),
                             f"compile setup leaked into {partition}")
            self.assertTrue(other["success"], other["output"])
            self.assertEqual(len(other["uploads"]), 1)
            records += other["sandbox"].records()
        self.assertEqual(ledger(records, ROWS, environments=True),
                         ledger(BASELINE["lanes"]["ignored-db-runtime"], ROWS, environments=True),
                         "compile setup changed original execution argv/env/replay multiplicities")
        self.assert_prep_failure_is_fatal(job)
        failed_generator = self.exercise(job, "other", generator_exit=42)
        self.assertFalse(failed_generator["success"])
        self.assertEqual([e["kind"] for e in failed_generator["events"]], ["generator"],
                         "setup ran despite failed command-reference generation")
        self.assertEqual(failed_generator["uploads"], [])
        return prep_index

    def test_cold_smoke_preparation_preserves_the_original_runtime_ledger(self):
        self.assert_contract(workflow()["ignored-db-runtime-partition"])

    def test_failed_compile_preparation_blocks_partition_and_receipt(self):
        self.assert_prep_failure_is_fatal(workflow()["ignored-db-runtime-partition"])

    def test_prior_setup_or_generator_failure_blocks_later_work(self):
        job = workflow()["ignored-db-runtime-partition"]
        for overrides in ({"prior_success": False}, {"generator_exit": 42}):
            with self.subTest(overrides=overrides):
                observed = self.exercise(job, "other", **overrides)
                self.assertFalse(observed["success"])
                self.assertNotIn(observed["run_index"], observed["attempts"])
                self.assertFalse(any(e["kind"] == "prepare" for e in observed["events"]))
                self.assertEqual(observed["uploads"], [])
                self.assertFalse(observed["receipt"].exists())

    def test_preparation_mutations_are_rejected_from_an_intact_workflow(self):
        job = workflow()["ignored-db-runtime-partition"]
        prep_index = self.assert_contract(job)

        def move_preparation(changed, *, before_generator=False):
            prep = changed["steps"].pop(prep_index)
            anchor = next(i for i, step in enumerate(changed["steps"])
                          if (step.get("name") == self.GENERATOR if before_generator
                              else step.get("id") == "run_partition"))
            changed["steps"].insert(anchor if before_generator else anchor + 1, prep)

        mutations = {
            "missing": lambda m: m["steps"].pop(prep_index),
            "disabled": lambda m: m["steps"][prep_index].__setitem__("if", "false"),
            "wrong owner": lambda m: m["steps"][prep_index].__setitem__("if", "matrix.partition == 'gateway'"),
            "every owner": lambda m: m["steps"][prep_index].pop("if", None),
            "nonfatal step": lambda m: m["steps"][prep_index].__setitem__("continue-on-error", True),
            "swallowed compile error": lambda m: m["steps"][prep_index].__setitem__("run", m["steps"][prep_index]["run"] + " || true"),
            "wrong target": lambda m: m["steps"][prep_index].__setitem__("run", "cargo test -p two-bot --locked --test restore_drill --no-run"),
            "wrong package": lambda m: m["steps"][prep_index].__setitem__("run", "cargo test -p two-bot-discord --locked --test alive --no-run"),
            "unlocked": lambda m: m["steps"][prep_index].__setitem__("run", "cargo test -p two-bot --test alive --no-run"),
            "wrong features": lambda m: m["steps"][prep_index].__setitem__("run", "cargo test -p two-bot --locked --test alive --features two-bot-core/db --no-run"),
            "wrong profile": lambda m: m["steps"][prep_index].__setitem__("run", "cargo test -p two-bot --locked --test alive --release --no-run"),
            "profile env drift": lambda m: m["steps"][prep_index].__setitem__("env", {"CARGO_PROFILE_TEST_DEBUG": "0"}),
            "unbounded": lambda m: m["steps"][prep_index].pop("timeout-minutes", None),
            "worker-sized bound": lambda m: m["steps"][prep_index].__setitem__("timeout-minutes", 30),
            "after execution": lambda m: move_preparation(m),
            "before generator": lambda m: move_preparation(m, before_generator=True),
            "runs tests instead of compiling only": lambda m: m["steps"][prep_index].__setitem__("run", "cargo test -p two-bot --locked --test alive -- --ignored --nocapture"),
            "bypasses failed generator": lambda m: m["steps"][prep_index].__setitem__("if", "always() && matrix.partition == 'other'"),
            "runner bypasses failed preparation": lambda m: next(s for s in m["steps"] if s.get("id") == "run_partition").__setitem__("if", "always()"),
            "upload bypasses failed preparation": lambda m: next(s for s in m["steps"] if s.get("name") == self.UPLOAD).__setitem__("if", "always()"),
            "preparation replaces execution": lambda m: next(s for s in m["steps"] if s.get("id") == "run_partition").__setitem__("run", "true"),
            "preparation writes an alternate receipt": lambda m: m["steps"][prep_index].__setitem__(
                "run", m["steps"][prep_index]["run"] + '\nmkdir -p "$RUNNER_TEMP/test-partition-receipts"\nprintf fixture > "$RUNNER_TEMP/test-partition-receipts/ignored-db-runtime-other.json"'),
        }
        for label, mutate in mutations.items():
            with self.subTest(mutation=label):
                changed = deepcopy(job)
                mutate(changed)
                with self.assertRaises(AssertionError, msg=label):
                    self.assert_contract(changed)


if __name__ == "__main__":
    unittest.main()
