"""Blind acceptance for three serial, package-wide ignored-bot coverage owners.

The gateway filter owns every name containing ``gateway_tests::``. The self-role
filter owns names containing ``self_role_runtime::`` but not ``gateway_tests::``;
``sweep-ignored-bot`` owns the catch-all. Libtest filters/skips are substrings,
not anchored Rust module prefixes. The two new public ids are listed in the
shared test support; all three must discover lib/bin/integration targets.

The real planner and real workflow selection shell execute. Only Cargo and
external tooling are doubled. The frozen baseline JSON is never regenerated;
new inventories extend it in memory. These are selection/feature/environment
controls, not compiled tests, database tests, speed measurements or LLVM parity.
"""

from collections import Counter
from copy import deepcopy
import hashlib
import json
from pathlib import Path
import shlex
import unittest

import test_critical_path_partitions_acceptance as partitions
from test_critical_path_support import (
    IGNORED_BOT_LEGS, Sandbox, assertions_message, ledger, metadata, run_coverage,
    selections,
)

GATEWAY, SELF_ROLE, REMAINDER = IGNORED_BOT_LEGS
BASELINE = partitions.BASELINE
IGNORED_SWEEP = next(record for record in BASELINE["lanes"]["coverage"]
                     if "--ignore-run-fail" in record["argv"]
                     and "--ignored" in record["argv"])
GLOBAL_SKIPS = tuple(IGNORED_SWEEP["argv"][index + 1]
                     for index, word in enumerate(IGNORED_SWEEP["argv"])
                     if word == "--skip")
# Literal owners independently qualify substring matching, including suffix and
# nested overlaps. Identical names in different Cargo targets remain distinct.
NAMED_CASES = (
    ("gateway_tests::future_gateway_case", GATEWAY),
    ("outer::gateway_tests::nested_case", GATEWAY),
    ("almost_gateway_tests::suffix_match", GATEWAY),
    ("gateway_tests::nested::self_role_runtime::case", GATEWAY),
    ("self_role_runtime::nested::gateway_tests::case", GATEWAY),
    ("self_role_runtime::future_self_role_case", SELF_ROLE),
    ("outer::self_role_runtime::nested_case", SELF_ROLE),
    ("almost_self_role_runtime::suffix_match", SELF_ROLE),
    ("unrelated::future_ignored_case", REMAINDER),
    ("gateway_tests_shadow::not_gateway", REMAINDER),
    ("self_role_runtime_shadow::not_self_role", REMAINDER),
)


def case_owner(name):
    if "gateway_tests::" in name:
        return GATEWAY
    if "self_role_runtime::" in name:
        return SELF_ROLE
    return REMAINDER


def fine_inventory():
    rows = partitions.evolving_inventory()
    targets = (("bin", "two-bot", []), ("lib", "two_bot", []),
               ("test", "ordinary_contract", []),
               ("test", "future_ignored_bot_contract", []),
               ("test", "feature_ignored_bot_contract", ["coverage-fixture"]))
    for kind, target, required in targets:
        for name, _ in NAMED_CASES:
            for ignored in (False, True):
                rows.append(dict(package="two-bot", kind=kind, target=target,
                                 name=name if ignored else name + "_nonignored",
                                 ignored=ignored, required_features=required))
    # Every global quarantine is exercised inside each selector, not only in
    # the remainder. Cycle targets to make the skip witnesses package-wide too.
    for index, skip in enumerate(GLOBAL_SKIPS):
        kind, target, required = targets[index % len(targets)]
        for prefix in ("gateway_tests::", "self_role_runtime::", "unrelated::"):
            rows.append(dict(package="two-bot", kind=kind, target=target,
                             name=prefix + skip, ignored=True, required_features=required))
    rows.append(dict(package="two-bot", kind="test", target="disabled_bot_contract",
                     name="gateway_tests::disabled_feature_case", ignored=True,
                     required_features=["disabled-fixture"]))
    # A same-named test in another package must retain its old package owner.
    rows.extend(dict(package=package, kind="test", target="cross_package_contract",
                     name=name, ignored=True)
                for package in ("two-bot-core", "two-bot-future") for name, _ in NAMED_CASES)
    return rows


def fine_metadata(rows):
    graph = metadata(rows)
    bot = next(package for package in graph["packages"] if package["name"] == "two-bot")
    bot["features"] = {"default": ["coverage-fixture"], "coverage-fixture": [],
                       "disabled-fixture": []}
    node = next(node for node in graph["resolve"]["nodes"] if node["id"] == bot["id"])
    node["features"] = ["coverage-fixture", "default"]
    return graph


def primary_ignored(records):
    return [record for record in records if "--ignore-run-fail" in record["argv"]
            and "--ignored" in record["argv"]]


def bot_ledger(records, rows, graph):
    return Counter({key: count for key, count in ledger(
        records, rows, graph=graph, environments=True).items() if key[0] == "two-bot"})


def assert_fine_owners(case, by_leg, rows, graph):
    wanted = bot_ledger([IGNORED_SWEEP], rows, graph)
    case.assertTrue(wanted, "the fixture must own real ignored-bot cases")
    actual = {leg_id: primary_ignored(records) for leg_id, records in by_leg.items()
              if bot_ledger(primary_ignored(records), rows, graph)}
    case.assertCountEqual(actual, IGNORED_BOT_LEGS,
                          "ignored-bot is still indivisible, missing an owner, or duplicated: "
                          "require gateway, ordered self-role and catch-all owners")
    combined = Counter()
    for leg_id in IGNORED_BOT_LEGS:
        records = actual[leg_id]
        units = ledger(records, rows, graph=graph, environments=True)
        expected = Counter({key: count for key, count in wanted.items()
                            if case_owner(key[3]) == leg_id})
        case.assertTrue(expected, leg_id + " has no owned-case witness")
        case.assertEqual(units, expected,
                         leg_id + " lost/repeated a case, narrowed targets, or drifted features/env/threads")
        case.assertTrue(all(unit[5] == "1" for unit in units), "ignored execution must remain serial")
        combined.update(units)
    case.assertEqual(combined, wanted, "fine owners must retain the frozen baseline multiplicities")
    case.assertTrue(all(count == 1 for count in combined.values()), "duplicate ignored sweep execution")


def command_controls():
    """Hand-derived valid command fixtures, not a production resolver oracle."""
    args = deepcopy(IGNORED_SWEEP["argv"])
    at = args.index("--workspace")
    args[at:at + 1] = ["-p", "two-bot"]
    split = args.index("--")
    cargo, harness = args[:split], args[split + 1:]
    groups = {
        GATEWAY: ["gateway_tests::"],
        SELF_ROLE: ["self_role_runtime::", "--skip", "gateway_tests::"],
        REMAINDER: ["--skip", "gateway_tests::", "--skip", "self_role_runtime::"],
    }
    return {leg_id: [dict(argv=cargo + ["--"] + filters + harness,
                          env=deepcopy(IGNORED_SWEEP["env"]))]
            for leg_id, filters in groups.items()}


def remove_skip(args, value):
    index = next(index for index in range(len(args) - 1)
                 if args[index:index + 2] == ["--skip", value])
    del args[index:index + 2]


class IgnoredBotOwnerOracleControls(unittest.TestCase):
    def setUp(self):
        self.rows = fine_inventory()
        self.graph = fine_metadata(self.rows)
        self.sandbox = Sandbox(self.rows, self.graph)
        self.addCleanup(self.sandbox.close)

    def execute(self, commands):
        observed = {}
        for leg_id, records in commands.items():
            self.sandbox.clear()
            for record in records:
                proc = self.sandbox.run(shlex.join(["cargo", *record["argv"]]), record["env"])
                self.assertEqual(proc.returncode, 0, assertions_message(proc))
            observed[leg_id] = self.sandbox.records()
        return observed

    def test_frozen_baseline_bytes_are_preserved(self):
        path = Path(partitions.__file__).with_name("test_critical_path_baseline.json")
        self.assertEqual(hashlib.sha256(path.read_bytes()).hexdigest(),
                         "00a0730f8f73ec9736963ff439a4a1d778a9910023d2605d99ab7d06ed79ed45")

    def test_literal_substring_owners_match_process_selections_across_every_target(self):
        observed = self.execute(command_controls())
        assert_fine_owners(self, observed, self.rows, self.graph)
        for name, owner in NAMED_CASES:
            with self.subTest(name=name):
                self.assertEqual(case_owner(name), owner)
                units = bot_ledger(observed[owner], self.rows, self.graph)
                self.assertEqual({(key[1], key[2]) for key in units if key[3] == name}, {
                    ("bin", "two-bot"), ("lib", "two_bot"), ("test", "ordinary_contract"),
                    ("test", "future_ignored_bot_contract"), ("test", "feature_ignored_bot_contract")})
                self.assertFalse(any(key[3].endswith("_nonignored") for key in units))
                self.assertFalse(any(key[2] == "disabled_bot_contract" for key in units))

    def test_explicit_llvm_cov_test_subcommand_is_not_a_positive_test_name_filter(self):
        controls = command_controls()
        for records in controls.values():
            records[0]["argv"].insert(1, "test")
        assert_fine_owners(self, self.execute(controls), self.rows, self.graph)

    def test_fake_cargo_rejects_unsupported_cargo_and_libtest_flags(self):
        for extra in (["--unsupported-cargo-flag", "--", "--ignored"],
                      ["--", "--ignored", "--unsupported-libtest-flag"]):
            with self.subTest(extra=extra):
                self.sandbox.clear()
                proc = self.sandbox.run(shlex.join([
                    "cargo", "llvm-cov", "test", "-p", "two-bot", "--locked", *extra]))
                self.assertEqual(proc.returncode, 101, assertions_message(proc))
                self.assertIn("unmodelled", proc.stderr)
                self.assertEqual(self.sandbox.records(), [])

    def test_duplicate_package_overlap_bin_only_missing_remainder_and_future_loss_mutants_are_killed(self):
        variants = {}
        whole = command_controls()
        for leg_id in IGNORED_BOT_LEGS:
            args = whole[leg_id][0]["argv"]
            split = args.index("--")
            args[split + 1:] = deepcopy(IGNORED_SWEEP["argv"][IGNORED_SWEEP["argv"].index("--") + 1:])
        variants["three-whole-package-sweeps"] = whole
        overlap = command_controls()
        remove_skip(overlap[SELF_ROLE][0]["argv"], "gateway_tests::")
        variants["overlapping-substrings-without-first-owner-skip"] = overlap
        bins = command_controls()
        for leg_id in (GATEWAY, SELF_ROLE):
            args = bins[leg_id][0]["argv"]
            for flag in ("--lib", "--bins", "--tests"):
                args.remove(flag)
            args[1:1] = ["--bin", "two-bot"]
        variants["binary-only-fine-groups"] = bins
        missing = command_controls()
        del missing[REMAINDER]
        variants["missing-catch-all"] = missing
        zero = command_controls()
        args = zero[GATEWAY][0]["argv"]
        args[args.index("--") + 1] = "gateway_tests::nonexistent_case"
        variants["successful-zero-test-gateway-is-not-evidence"] = zero
        closed = command_controls()
        for records in closed.values():
            args = records[0]["argv"]
            args[1:1] = ["--test", "ordinary_contract"]
        variants["closed-integration-target-list-loses-future-cases"] = closed
        for kind, commands in variants.items():
            with self.subTest(mutant=kind):
                observed = self.execute(commands)
                with self.assertRaises(AssertionError):
                    assert_fine_owners(self, observed, self.rows, self.graph)

    def test_each_global_quarantine_is_effective_in_all_three_groups(self):
        controls = command_controls()
        # These are execution-ledger mutations, not a source-text skip count.
        # Every removed skip must expose its sentinel in that particular owner.
        for leg_id in IGNORED_BOT_LEGS:
            for skip in GLOBAL_SKIPS:
                with self.subTest(owner=leg_id, removed_skip=skip):
                    bad = deepcopy(controls)
                    remove_skip(bad[leg_id][0]["argv"], skip)
                    with self.assertRaises(AssertionError):
                        assert_fine_owners(self, bad, self.rows, self.graph)

    def test_serial_feature_and_disposable_environment_drift_mutants_are_killed(self):
        variants = {}
        threads = command_controls()
        args = threads[SELF_ROLE][0]["argv"]
        args[args.index("--test-threads=1")] = "--test-threads=2"
        variants["parallel-libtest-process"] = threads
        features = command_controls()
        args = features[GATEWAY][0]["argv"]
        args[1:1] = ["--no-default-features"]
        variants["lost-workspace-default-feature-and-required-target"] = features
        for key, value in (("TWO_TEST_DATABASE_URL", "postgres://agent_test:@agent-testdb:5432/two_bot_test_wrong"),
                           ("TWO_DATABASE_TLS", "wrong-fixture-mode"), ("MEMBER_TESTDB", "wrong-fixture-host")):
            bad = command_controls()
            bad[REMAINDER][0]["env"][key] = value
            variants["environment-" + key] = bad
        for kind, commands in variants.items():
            with self.subTest(mutant=kind):
                with self.assertRaises(AssertionError):
                    assert_fine_owners(self, self.execute(commands), self.rows, self.graph)


class IgnoredBotCoverageOwnersAcceptance(unittest.TestCase):
    def setUp(self):
        self.rows = fine_inventory()
        self.graph = fine_metadata(self.rows)
        self.sandbox = Sandbox(self.rows, self.graph)
        self.addCleanup(self.sandbox.close)
        self.planner = partitions.planner()

    def executions(self):
        observed = {}
        for leg in self.planner.FULL_LEGS:
            self.sandbox.clear()
            proc = run_coverage(self.sandbox, [leg["id"]])
            self.assertEqual(proc.returncode, 0, leg["id"] + "\n" + assertions_message(proc))
            observed[leg["id"]] = self.sandbox.records()
        return observed

    def test_actual_workflow_has_three_package_wide_serial_ignored_bot_owners(self):
        assert_fine_owners(self, self.executions(), self.rows, self.graph)

    def test_actual_workflow_preserves_full_frozen_features_env_and_dedicated_replay_multiplicities(self):
        actual = [record for records in self.executions().values() for record in records]
        self.assertEqual(ledger(actual, self.rows, graph=self.graph, environments=True),
                         ledger(BASELINE["lanes"]["coverage"], self.rows,
                                graph=self.graph, environments=True))

    def test_all_three_selected_together_execute_exactly_once_through_the_real_selection_shell(self):
        self.sandbox.clear()
        proc = run_coverage(self.sandbox, IGNORED_BOT_LEGS)
        self.assertEqual(proc.returncode, 0,
                         "the actual shell cannot execute the finer ignored-bot selection\n" + assertions_message(proc))
        actual = primary_ignored(self.sandbox.records())
        self.assertEqual(bot_ledger(actual, self.rows, self.graph),
                         bot_ledger([IGNORED_SWEEP], self.rows, self.graph))
        self.assertEqual(len(actual), 3, "each fine owner must execute once, not duplicate the package sweep")

    def test_real_resolved_descriptors_bind_every_fine_invocation_and_package_owner(self):
        ids = {leg["id"] for leg in self.planner.FULL_LEGS}
        self.assertTrue(set(IGNORED_BOT_LEGS) <= ids, "planner still exposes only the coarse ignored-bot owner")
        proc = self.sandbox.run("python3 scripts/ci/plan-coverage-shards.py --describe-sweeps")
        self.assertEqual(proc.returncode, 0, assertions_message(proc))
        descriptors = {leg["id"]: leg for leg in json.loads(proc.stdout)}
        actual = self.executions()
        for leg_id in IGNORED_BOT_LEGS:
            with self.subTest(leg=leg_id):
                leg = descriptors[leg_id]
                self.assertEqual(leg["crates"], ["two-bot"])
                commands = [leg["command"], *leg.get("supplemental_commands", [])]
                self.assertEqual([shlex.split(command)[1:] for command in commands],
                                 [record["argv"] for record in actual[leg_id]])
        assert_fine_owners(self, actual, self.rows, self.graph)

    def test_default_full_plan_places_the_three_costly_groups_on_distinct_eight_workers(self):
        value = self.planner.plan(["Cargo.toml"], 8, event_name="pull_request",
                                  ref="refs/pull/1/merge", metadata=self.graph)
        self.assertTrue(value["widened"])
        self.assertEqual(len(value["shards"]), 8)
        self.assertTrue(set(IGNORED_BOT_LEGS) <= set(value["legs"]), "full plan lacks the finer ignored-bot owners")
        owners = [index for leg in IGNORED_BOT_LEGS for index, shard in enumerate(value["shards"]) if leg in shard]
        self.assertEqual(len(owners), 3)
        self.assertEqual(len(set(owners)), 3, "costly ignored-bot groups still share a critical-path worker")
        self.assertCountEqual([leg for shard in value["shards"] for leg in shard], value["legs"])

    def test_bot_edits_and_transitive_dependency_edits_select_every_necessary_bot_group(self):
        for crate in ("bot", "core", "discord", "cutover", "store"):
            with self.subTest(crate=crate):
                value = self.planner.plan([f"crates/{crate}/src/lib.rs"], 8,
                                          event_name="pull_request", ref="refs/pull/1/merge", metadata=self.graph)
                self.assertFalse(value["widened"])
                self.assertTrue(set(IGNORED_BOT_LEGS) <= set(value["legs"]),
                                crate + " edit lost a bot or reverse-dependent ignored group")
                self.assertCountEqual([leg for shard in value["shards"] for leg in shard], value["legs"])

    def test_workspace_main_schedule_unknown_and_incomplete_graphs_conservatively_keep_all_groups(self):
        incomplete = deepcopy(self.graph)
        incomplete["resolve"]["nodes"].pop()
        for paths, event, ref, graph in (
            (["Cargo.lock"], "pull_request", "refs/pull/1/merge", self.graph),
            (["unknown/input.rs"], "pull_request", "refs/pull/1/merge", self.graph),
            (["README.md"], "push", "refs/heads/main", self.graph),
            (["README.md"], "schedule", "refs/heads/main", self.graph),
            (["crates/core/src/lib.rs"], "pull_request", "refs/pull/1/merge", incomplete),
        ):
            with self.subTest(paths=paths, event=event):
                value = self.planner.plan(paths, 8, event_name=event, ref=ref, metadata=graph)
                self.assertTrue(value["widened"])
                self.assertTrue(set(IGNORED_BOT_LEGS) <= set(value["legs"]), "widening lost a fine bot group")
                self.assertCountEqual(value["legs"], [leg["id"] for leg in self.planner.FULL_LEGS])
                self.assertCountEqual([leg for shard in value["shards"] for leg in shard], value["legs"])

    def test_malformed_fine_selections_fail_before_any_partial_measurement(self):
        for ids in ([], [REMAINDER, REMAINDER], [REMAINDER, "unknown-fine-owner"],
                    [GATEWAY, SELF_ROLE, REMAINDER, "sweep-ignored-bot-unknown"]):
            with self.subTest(ids=ids):
                self.sandbox.clear()
                proc = run_coverage(self.sandbox, ids)
                self.assertNotEqual(proc.returncode, 0, "malformed selection silently narrowed the ignored sweep")
                self.assertEqual(self.sandbox.records(), [], "invalid selection executed partial work")


if __name__ == "__main__":
    unittest.main()
