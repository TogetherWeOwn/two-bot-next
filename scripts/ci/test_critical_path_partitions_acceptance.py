"""Independent RED acceptance for critical-path partitions (baseline 38e84d2).

Small execution interface for the two formerly monolithic test lanes:

* scripts/ci/run-test-partition.py --lane rust-tests|ignored-db-runtime --list
  prints {"include": [{"partition": "nonempty-id", ...}, ...]} JSON. IDs are
  unique; each lane has at least two nonempty partitions. The scheduling and
  target/selector discovery algorithms are intentionally unspecified.
* The same CLI with --partition ID executes exactly that partition. Missing,
  empty, unknown or repeated selections fail before executing tests.
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

import importlib.util
import json
import re
import shlex
import sys
import unittest
from collections import Counter
from pathlib import Path

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
    return ROWS + extra


def planner():
    spec = importlib.util.spec_from_file_location("critical_path_coverage_planner",
                                                 ROOT / "scripts/ci/plan-coverage-shards.py")
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
                    ("two-bot-core/db",), None, False)
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


if __name__ == "__main__":
    unittest.main()
