"""Partition-routing acceptance for the test-inventory gate.

Every integration test target must be executed by some CI job. Partitioned
lanes execute via ``scripts/ci/run-test-partition.py`` behind a
planner/worker/stable-final matrix; this file pins the smallest executed
routing contract:

* Worker steps execute the runner with ``--lane "$CI_TEST_LANE"
  --partition "$CI_TEST_PARTITION"``, where ``CI_TEST_LANE`` is a literal
  lane and ``CI_TEST_PARTITION`` is ``${{ matrix.partition }}``. Echo,
  comments, display names, env values, ``if`` expressions, ``--help`` or a
  bare script name are mentions, not execution.
* The planner executes ``--list`` and publishes per-lane outputs; each
  worker matrix consumes its lane via ``fromJSON(needs.<planner>.outputs.
  <lane>)`` and needs the planner plus job inputs.
* Each stable final needs its worker, requires successful partitions
  (``always()`` guard plus a strict require step), and executes
  ``--verify-receipts`` for its lane. ``ci-ok`` needs the planner,
  workers and finals.
* Package identity matters: the runtime lane owns ``two-bot/alive``
  (``crates/bot/tests/alive.rs`` with a ``two-bot`` manifest). The same
  target name in another package is unowned.
* Unignored targets are covered only by executed Cargo-metadata discovery
  and dispatch in the runner; ignored targets only by an executed
  ``--test`` owner for their package. Comments, unused literals, decoy
  strings, empty discovery (``return []``) or removed dispatch do not
  route, even when the file still mentions the names.

Fixtures build a miniature repo with a connected workflow and a copy of
the real runner, stubbing only the asserted execution. No Cargo build,
database, network or workflow dispatch occurs. Diagnostics keep the
existing ``orphaned test target`` / ``missing`` shape with the ignored
kind preserved.
"""

import ast
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = Path(__file__).with_name("check-test-inventory.py")
REAL_RUNNER = ROOT / "scripts/ci/run-test-partition.py"
REAL_RUNNER_TEXT = REAL_RUNNER.read_text()

import importlib.util

SPEC = importlib.util.spec_from_file_location("inventory_gate_partition_routing", SCRIPT)
gate = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(gate)


FUZZ_STEP = "      - run: cargo +nightly-2026-10-01 fuzz build\n"
ALL_TARGETS_STEP = "      - run: cargo clippy --workspace --all-targets --locked\n"
LIB_BINS_STEP = "      - run: cargo test --workspace --lib --bins --locked\n"
DOC_STEP = "      - run: cargo test --workspace --doc --locked\n"

NIGHTLY_YML = """jobs:
  sweep:
    steps:
      - run: cargo test --workspace --locked
"""

REQUIRE_STEP = """      - name: Require every test partition
        env:
          PARTITION_RESULTS: ${{ toJSON(needs) }}
        run: |
          python3 - <<'PY'
          import json
          import os
          import sys
          results = json.loads(os.environ['PARTITION_RESULTS'])
          if not results or any(row.get('result') != 'success' for row in results.values()):
              print('::error::required test partition did not succeed; failing closed')
              sys.exit(1)
          print('All required test partitions succeeded')
          PY
"""

# Connected path for both lanes. Omits direct lib/bins, doc and blanket
# runs: those come from the runner.
VALID_CHECK_YML = """jobs:
  job-inputs:
    outputs:
      rust: 'true'
    steps:
      - run: true
  test-partition-plan:
    needs: [job-inputs]
    if: needs.job-inputs.outputs.rust != 'false'
    outputs:
      rust-tests: ${{ steps.plan.outputs.rust-tests }}
      ignored-db-runtime: ${{ steps.plan.outputs.ignored-db-runtime }}
    steps:
      - id: plan
        run: |
          set -euo pipefail
          for lane in rust-tests ignored-db-runtime; do
            matrix=$(python3 scripts/ci/run-test-partition.py --lane "$lane" --list)
            printf '%s=%s\\n' "$lane" "$matrix" >> "$GITHUB_OUTPUT"
          done
  rust-tests-partition:
    needs: [job-inputs, test-partition-plan]
    if: needs.job-inputs.outputs.rust != 'false'
    strategy:
      matrix: ${{ fromJSON(needs.test-partition-plan.outputs.rust-tests) }}
    steps:
      - name: Run selected test partition
        id: run_partition
        env:
          CI_TEST_LANE: rust-tests
          CI_TEST_PARTITION: ${{ matrix.partition }}
        run: python3 scripts/ci/run-test-partition.py --lane "$CI_TEST_LANE" --partition "$CI_TEST_PARTITION"
  rust-tests:
    needs: [job-inputs, rust-tests-partition]
    if: always() && needs.job-inputs.outputs.rust != 'false'
    steps:
""" + REQUIRE_STEP + """      - name: Verify exact successful partition receipts and property budget
        env:
          CI_TEST_LANE: rust-tests
        run: python3 scripts/ci/run-test-partition.py --lane "$CI_TEST_LANE" --verify-receipts test-partition-receipts
  ignored-db-runtime-partition:
    needs: [job-inputs, test-partition-plan]
    if: needs.job-inputs.outputs.rust != 'false'
    strategy:
      matrix: ${{ fromJSON(needs.test-partition-plan.outputs.ignored-db-runtime) }}
    steps:
      - name: Run selected test partition
        id: run_partition
        env:
          CI_TEST_LANE: ignored-db-runtime
          CI_TEST_PARTITION: ${{ matrix.partition }}
        run: python3 scripts/ci/run-test-partition.py --lane "$CI_TEST_LANE" --partition "$CI_TEST_PARTITION"
  ignored-db-runtime:
    needs: [job-inputs, ignored-db-runtime-partition]
    if: always() && needs.job-inputs.outputs.rust != 'false'
    steps:
""" + REQUIRE_STEP + """      - name: Verify exact successful partition receipts and property budget
        env:
          CI_TEST_LANE: ignored-db-runtime
        run: python3 scripts/ci/run-test-partition.py --lane "$CI_TEST_LANE" --verify-receipts test-partition-receipts
  check:
    steps:
""" + FUZZ_STEP + ALL_TARGETS_STEP + """  ci-ok:
    needs: [job-inputs, test-partition-plan, rust-tests-partition, ignored-db-runtime-partition, rust-tests, ignored-db-runtime, check]
    if: ${{ always() }}
    steps:
""" + REQUIRE_STEP

DIRECT_CHECK_YML = """jobs:
  check:
    steps:
""" + LIB_BINS_STEP + DOC_STEP + ALL_TARGETS_STEP + FUZZ_STEP + """      - run: cargo test --workspace --test '*' --locked
      - run: cargo test -p my-crate --test alive --locked -- --ignored
  ci-ok:
    needs: [check]
    steps:
      - run: true
"""

PLAIN_SRC = "#[test]\nfn it() {}\n"
IGNORED_SRC = "#[ignore]\n#[test]\nfn it() {}\n"
BOT_MANIFEST = '[package]\nname = "two-bot"\nversion = "0.4.0"\n'
MYCRATE_MANIFEST = '[package]\nname = "my-crate"\nversion = "0.4.0"\n'
OTHER_MANIFEST = '[package]\nname = "other-crate"\nversion = "0.4.0"\n'

BOT_ALIVE = {
    "crates/bot/tests/alive.rs": IGNORED_SRC,
    "crates/bot/Cargo.toml": BOT_MANIFEST,
}
MYCRATE_PLAIN = {
    "crates/my-crate/tests/plain.rs": PLAIN_SRC,
    "crates/my-crate/Cargo.toml": MYCRATE_MANIFEST,
}
MYCRATE_FUTURE = {
    "crates/my-crate/tests/future_new.rs": PLAIN_SRC,
    "crates/my-crate/Cargo.toml": MYCRATE_MANIFEST,
}
MYCRATE_ALIVE = {
    "crates/my-crate/tests/alive.rs": IGNORED_SRC,
    "crates/my-crate/Cargo.toml": MYCRATE_MANIFEST,
}
OTHER_ALIVE = {
    "crates/other/tests/alive.rs": IGNORED_SRC,
    "crates/other/Cargo.toml": OTHER_MANIFEST,
}


def make_root(check_yml, nightly_yml, test_files, runner_text=REAL_RUNNER_TEXT):
    tmp = tempfile.TemporaryDirectory()
    root = Path(tmp.name)
    files = {
        ".github/workflows/check.yml": check_yml,
        ".github/workflows/nightly.yml": nightly_yml,
        "fuzz/Cargo.toml": '[[bin]]\nname = "wiggle"\npath = "fuzz_targets/wiggle.rs"\n',
        "fuzz/fuzz_targets/wiggle.rs": "// target\n",
    }
    files.update(test_files)
    for path, content in files.items():
        full = root / path
        full.parent.mkdir(parents=True, exist_ok=True)
        full.write_text(content)
    if runner_text is not None:
        runner = root / "scripts/ci/run-test-partition.py"
        runner.parent.mkdir(parents=True, exist_ok=True)
        runner.write_text(runner_text)
    return tmp, root


def check_fixture(check_yml, nightly_yml, test_files, runner_text=REAL_RUNNER_TEXT):
    tmp, root = make_root(check_yml, nightly_yml, test_files, runner_text)
    try:
        return gate.check(root)
    finally:
        tmp.cleanup()


def with_direct_markers(check_yml):
    # Isolate routing verdicts: markers pass directly so only the target
    # under test can fail. Valid fixtures omit these.
    return check_yml.replace(ALL_TARGETS_STEP, ALL_TARGETS_STEP + LIB_BINS_STEP + DOC_STEP)


def assert_orphaned(case, errors, name, ignored):
    case.assertTrue(
        any(name in error and "orphaned" in error for error in errors),
        f"expected {name} orphaned, got: {errors}")
    match = next(error for error in errors if name in error)
    kind = "has #[ignore]d tests" if ignored else "not referenced"
    case.assertIn(kind, match, f"diagnostic lost its ignored kind: {match}")


def parse_jobs(check_yml):
    return yaml.safe_load(check_yml)["jobs"]


def worker_step(job):
    return next(s for s in job.get("steps", []) if s.get("id") == "run_partition")


def assert_valid_dag(case, jobs):
    case.assertIn("rust", jobs["job-inputs"]["outputs"])
    planner = jobs["test-partition-plan"]
    case.assertIn("job-inputs", planner["needs"])
    case.assertIn("rust", planner["if"])
    case.assertIn("rust-tests", planner["outputs"])
    case.assertIn("ignored-db-runtime", planner["outputs"])
    plan = next(s for s in planner["steps"] if s.get("id") == "plan")
    case.assertIn("--list", plan["run"])
    for lane, worker_id, final_id in (
        ("rust-tests", "rust-tests-partition", "rust-tests"),
        ("ignored-db-runtime", "ignored-db-runtime-partition", "ignored-db-runtime"),
    ):
        worker = jobs[worker_id]
        case.assertIn("test-partition-plan", worker["needs"])
        case.assertIn("job-inputs", worker["needs"])
        case.assertEqual(
            worker["strategy"]["matrix"],
            "${{ fromJSON(needs.test-partition-plan.outputs." + lane + ") }}")
        step = worker_step(worker)
        env = worker.get("env", {}) | step.get("env", {})
        case.assertEqual(env["CI_TEST_LANE"], lane)
        case.assertEqual(env["CI_TEST_PARTITION"], "${{ matrix.partition }}")
        case.assertIn("--lane", step["run"])
        case.assertIn("--partition", step["run"])
        final = jobs[final_id]
        case.assertIn(worker_id, final["needs"])
        case.assertIn("always()", final["if"])
        names = [s.get("name", "") for s in final["steps"]]
        case.assertIn("Require every test partition", names)
        verify = next(s for s in final["steps"] if "Verify" in s.get("name", ""))
        verify_env = final.get("env", {}) | verify.get("env", {})
        case.assertEqual(verify_env["CI_TEST_LANE"], lane)
        case.assertIn("--verify-receipts", verify["run"])
    needs = jobs["ci-ok"]["needs"]
    for job_id in ("test-partition-plan", "rust-tests-partition",
                   "ignored-db-runtime-partition", "rust-tests", "ignored-db-runtime"):
        case.assertIn(job_id, needs)


class ValidRunnerRoutingTests(unittest.TestCase):
    """Genuine runner paths must route."""

    def test_rust_runner_covers_unignored_and_markers(self):
        assert_valid_dag(self, parse_jobs(VALID_CHECK_YML))
        errors = check_fixture(VALID_CHECK_YML, NIGHTLY_YML, dict(MYCRATE_PLAIN))
        self.assertEqual(errors, [], f"rust-tests runner should route plain: {errors}")

    def test_rust_runner_covers_future_unlisted_unignored(self):
        assert_valid_dag(self, parse_jobs(VALID_CHECK_YML))
        errors = check_fixture(VALID_CHECK_YML, NIGHTLY_YML, dict(MYCRATE_FUTURE))
        self.assertEqual(errors, [], f"dynamic discovery should route future_new: {errors}")

    def test_runtime_runner_covers_owned_ignored_alive(self):
        assert_valid_dag(self, parse_jobs(VALID_CHECK_YML))
        errors = check_fixture(VALID_CHECK_YML, NIGHTLY_YML, dict(BOT_ALIVE))
        self.assertEqual(errors, [], f"runtime runner should route two-bot/alive: {errors}")

    def test_both_lanes_cover_mixed_owned_tree(self):
        assert_valid_dag(self, parse_jobs(VALID_CHECK_YML))
        files = dict(MYCRATE_PLAIN) | dict(MYCRATE_FUTURE) | dict(BOT_ALIVE)
        errors = check_fixture(VALID_CHECK_YML, NIGHTLY_YML, files)
        self.assertEqual(errors, [], f"both lanes should route the owned tree: {errors}")

    def test_cli_passes_valid_runner_tree(self):
        assert_valid_dag(self, parse_jobs(VALID_CHECK_YML))
        tmp, root = make_root(VALID_CHECK_YML, NIGHTLY_YML, dict(BOT_ALIVE))
        try:
            result = subprocess.run(
                [sys.executable, str(SCRIPT), "--root", str(root)],
                capture_output=True, text=True, timeout=60)
        finally:
            tmp.cleanup()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("every target is routed", result.stdout)


class DirectCargoControlsTests(unittest.TestCase):
    def test_direct_test_command_still_routed(self):
        files = dict(MYCRATE_PLAIN) | dict(MYCRATE_ALIVE)
        errors = check_fixture(DIRECT_CHECK_YML, NIGHTLY_YML, files, runner_text=None)
        self.assertEqual(errors, [])

    def test_direct_markers_still_routed(self):
        errors = check_fixture(
            DIRECT_CHECK_YML, NIGHTLY_YML, dict(MYCRATE_PLAIN), runner_text=None)
        self.assertEqual(errors, [])

    def test_removing_direct_command_flips_to_orphaned(self):
        broken = DIRECT_CHECK_YML.replace(
            "cargo test -p my-crate --test alive --locked -- --ignored", "true")
        errors = check_fixture(
            broken, NIGHTLY_YML, dict(MYCRATE_ALIVE), runner_text=None)
        assert_orphaned(self, errors, "alive", True)

    def test_cli_passes_direct_tree(self):
        tmp, root = make_root(DIRECT_CHECK_YML, NIGHTLY_YML, dict(MYCRATE_ALIVE), runner_text=None)
        try:
            result = subprocess.run(
                [sys.executable, str(SCRIPT), "--root", str(root)],
                capture_output=True, text=True, timeout=60)
        finally:
            tmp.cleanup()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)


class MentionOnlyRunnerRejectedTests(unittest.TestCase):
    """Script-name mentions without execution must not route."""

    def test_mention_kinds_do_not_route(self):
        worker_run = 'run: python3 scripts/ci/run-test-partition.py --lane "$CI_TEST_LANE" --partition "$CI_TEST_PARTITION"'
        run_snippets = {
            "echo": 'run: echo "python3 scripts/ci/run-test-partition.py --lane rust-tests --partition integration-0"',
            "trailing_comment": "run: true # python3 scripts/ci/run-test-partition.py --lane rust-tests --partition integration-0",
            "help_only": "run: python3 scripts/ci/run-test-partition.py --help",
            "bare_name": "run: ls scripts/ci/run-test-partition.py",
        }
        for kind, snippet in run_snippets.items():
            with self.subTest(kind=kind):
                check_yml = with_direct_markers(VALID_CHECK_YML.replace(worker_run, snippet))
                self.assertIn("run-test-partition", check_yml)
                jobs = parse_jobs(check_yml)
                runs = [
                    s.get("run", "")
                    for _, j in jobs.items()
                    for s in j.get("steps", [])
                    if isinstance(s.get("run", ""), str)
                ]
                self.assertFalse(any("--partition" in r and "run-test-partition" in r and "echo" not in r for r in runs))
                errors = check_fixture(check_yml, NIGHTLY_YML, dict(BOT_ALIVE))
                assert_orphaned(self, errors, "alive", True)

    def test_unreachable_worker_or_verifier_after_exit_does_not_route(self):
        # The supported worker/verifier interface is one direct command,
        # not a general shell reachability proof. A valid argv after an
        # unconditional successful exit must not satisfy that interface.
        commands = {
            "worker": 'python3 scripts/ci/run-test-partition.py --lane "$CI_TEST_LANE" --partition "$CI_TEST_PARTITION"',
            "verifier": 'python3 scripts/ci/run-test-partition.py --lane "$CI_TEST_LANE" --verify-receipts test-partition-receipts',
        }
        for role, command in commands.items():
            self.assertEqual(VALID_CHECK_YML.count("run: " + command), 2)
            unreachable = "run: |\n          exit 0\n          " + command
            check_yml = with_direct_markers(
                VALID_CHECK_YML.replace("run: " + command, unreachable))
            for files, name, ignored in (
                (MYCRATE_PLAIN, "plain", False),
                (BOT_ALIVE, "alive", True),
            ):
                with self.subTest(role=role, target=name):
                    errors = check_fixture(check_yml, NIGHTLY_YML, dict(files))
                    assert_orphaned(self, errors, name, ignored)

    def test_non_run_mentions_do_not_route(self):
        worker_run = 'run: python3 scripts/ci/run-test-partition.py --lane "$CI_TEST_LANE" --partition "$CI_TEST_PARTITION"'
        mention_steps = {
            "step_name": "      - name: python3 scripts/ci/run-test-partition.py --lane rust-tests --partition integration-0\n        run: true\n",
            "step_env": '      - run: true\n        env:\n          CMD: "python3 scripts/ci/run-test-partition.py --lane rust-tests --partition integration-0"\n',
            "if_only": "      - if: ${{ matrix.partition == 'integration-0' }}\n        run: true\n",
        }
        for kind, step in mention_steps.items():
            with self.subTest(kind=kind):
                no_worker = VALID_CHECK_YML.replace(worker_run, "run: true")
                check_yml = no_worker.replace("  check:\n    steps:\n", "  check:\n    steps:\n" + step, 1)
                check_yml = with_direct_markers(check_yml)
                self.assertIn("run-test-partition", check_yml)
                errors = check_fixture(check_yml, NIGHTLY_YML, dict(BOT_ALIVE))
                assert_orphaned(self, errors, "alive", True)

    def test_shell_comment_in_run_block_does_not_route(self):
        worker_run = 'run: python3 scripts/ci/run-test-partition.py --lane "$CI_TEST_LANE" --partition "$CI_TEST_PARTITION"'
        block = ("run: |\n"
                 "          # python3 scripts/ci/run-test-partition.py --lane rust-tests --partition integration-0\n"
                 "          true")
        check_yml = with_direct_markers(VALID_CHECK_YML.replace(worker_run, block))
        errors = check_fixture(check_yml, NIGHTLY_YML, dict(BOT_ALIVE))
        assert_orphaned(self, errors, "alive", True)


class LanePartitionSelectorRejectedTests(unittest.TestCase):
    def test_broken_selectors_do_not_route(self):
        cases = {
            "missing_lane_env": VALID_CHECK_YML.replace("          CI_TEST_LANE: rust-tests\n", ""),
            "wrong_lane_literal": VALID_CHECK_YML.replace("CI_TEST_LANE: rust-tests", "CI_TEST_LANE: wrong-lane"),
            "missing_partition_env": VALID_CHECK_YML.replace("          CI_TEST_PARTITION: ${{ matrix.partition }}\n", ""),
            "hardcoded_partition": VALID_CHECK_YML.replace(
                "CI_TEST_PARTITION: ${{ matrix.partition }}", "CI_TEST_PARTITION: integration-0"),
            "worker_missing_partition_arg": VALID_CHECK_YML.replace(
                ' --partition "$CI_TEST_PARTITION"', ""),
            "worker_missing_lane_arg": VALID_CHECK_YML.replace(
                ' --lane "$CI_TEST_LANE"', ""),
        }
        for kind, check_yml in cases.items():
            with self.subTest(kind=kind):
                jobs = parse_jobs(with_direct_markers(check_yml))
                step = worker_step(jobs["rust-tests-partition"])
                env = jobs["rust-tests-partition"].get("env", {}) | step.get("env", {})
                if kind == "missing_lane_env":
                    self.assertNotIn("CI_TEST_LANE", env)
                elif kind == "wrong_lane_literal":
                    self.assertEqual(env["CI_TEST_LANE"], "wrong-lane")
                errors = check_fixture(
                    with_direct_markers(check_yml), NIGHTLY_YML, dict(BOT_ALIVE))
                assert_orphaned(self, errors, "alive", True)


class DisconnectedPathRejectedTests(unittest.TestCase):
    def test_broken_connections_do_not_route(self):
        literal_matrix = VALID_CHECK_YML.replace(
            "matrix: ${{ fromJSON(needs.test-partition-plan.outputs.rust-tests) }}",
            'matrix: {"include": [{"partition": "integration-0"}]}')
        cases = {
            "missing_planner_list": VALID_CHECK_YML.replace(
                'matrix=$(python3 scripts/ci/run-test-partition.py --lane "$lane" --list)',
                'matrix=$(echo "$lane")'),
            "worker_matrix_not_from_planner": literal_matrix,
            "missing_stable_final_verify": VALID_CHECK_YML.replace(
                'run-test-partition.py --lane "$CI_TEST_LANE" --verify-receipts test-partition-receipts',
                'echo verify-receipts'),
            "missing_ci_ok_needs": VALID_CHECK_YML.replace(
                "needs: [job-inputs, test-partition-plan, rust-tests-partition, ignored-db-runtime-partition, rust-tests, ignored-db-runtime, check]",
                "needs: [job-inputs, check]"),
        }
        for kind, check_yml in cases.items():
            with self.subTest(kind=kind):
                jobs = parse_jobs(check_yml)
                if kind == "missing_ci_ok_needs":
                    self.assertNotIn("rust-tests-partition", jobs["ci-ok"]["needs"])
                elif kind == "worker_matrix_not_from_planner":
                    self.assertNotIn("fromJSON", str(jobs["rust-tests-partition"]["strategy"]["matrix"]))
                errors = check_fixture(
                    with_direct_markers(check_yml), NIGHTLY_YML, dict(BOT_ALIVE))
                assert_orphaned(self, errors, "alive", True)

    def test_missing_require_step_does_not_route(self):
        check_yml = VALID_CHECK_YML.replace(REQUIRE_STEP, "")
        jobs = parse_jobs(check_yml)
        names = [s.get("name", "") for s in jobs["rust-tests"]["steps"]]
        self.assertNotIn("Require every test partition", names)
        errors = check_fixture(with_direct_markers(check_yml), NIGHTLY_YML, dict(BOT_ALIVE))
        assert_orphaned(self, errors, "alive", True)

    def test_missing_always_guard_does_not_route(self):
        check_yml = VALID_CHECK_YML.replace(
            "if: always() && needs.job-inputs.outputs.rust != 'false'",
            "if: needs.job-inputs.outputs.rust != 'false'")
        jobs = parse_jobs(check_yml)
        self.assertNotIn("always()", jobs["rust-tests"]["if"])
        errors = check_fixture(with_direct_markers(check_yml), NIGHTLY_YML, dict(BOT_ALIVE))
        assert_orphaned(self, errors, "alive", True)

    def test_missing_job_inputs_job_is_rejected(self):
        check_yml = VALID_CHECK_YML.replace(
            "  job-inputs:\n    outputs:\n      rust: 'true'\n    steps:\n      - run: true\n", "")
        jobs = parse_jobs(check_yml)
        self.assertNotIn("job-inputs", jobs)
        self.assertIn("job-inputs", jobs["test-partition-plan"]["needs"])
        errors = check_fixture(with_direct_markers(check_yml), NIGHTLY_YML, dict(BOT_ALIVE))
        assert_orphaned(self, errors, "alive", True)

    def test_nightly_only_runner_does_not_route(self):
        check_yml = with_direct_markers("""jobs:
  check:
    steps:
""" + FUZZ_STEP + ALL_TARGETS_STEP + """  ci-ok:
    needs: [check]
    steps:
      - run: true
""")
        errors = check_fixture(check_yml, VALID_CHECK_YML, dict(BOT_ALIVE))
        assert_orphaned(self, errors, "alive", True)

    def test_missing_runner_file_is_orphaned_not_a_crash(self):
        errors = check_fixture(
            with_direct_markers(VALID_CHECK_YML), NIGHTLY_YML, dict(BOT_ALIVE), runner_text=None)
        assert_orphaned(self, errors, "alive", True)

    def test_broken_runner_does_not_cover_lib_bins_or_doc_markers(self):
        broken = VALID_CHECK_YML.replace(
            'matrix=$(python3 scripts/ci/run-test-partition.py --lane "$lane" --list)',
            'matrix=$(echo "$lane")')
        errors = check_fixture(broken, NIGHTLY_YML, dict(MYCRATE_PLAIN))
        self.assertTrue(
            any("workspace lib/bins unit run" in error for error in errors),
            f"broken runner must not cover lib/bins: {errors}")
        self.assertTrue(
            any("workspace doc-test run" in error for error in errors),
            f"broken runner must not cover doc: {errors}")


def break_metadata_call(runner_text):
    return runner_text.replace('"cargo", "metadata"', '"cargo", "REDACTED"')


def with_metadata_decoys(runner_text):
    return '# cargo metadata dynamic discovery\nunused_metadata = "cargo metadata"\n' + runner_text


def break_alive_owner(runner_text):
    return runner_text.replace('"alive"', '"redacted_alive"')


def with_alive_decoys(runner_text):
    return '# alive owner\ndecoy_alive = "alive"\n' + runner_text


class OwnershipAndDiscoveryTests(unittest.TestCase):
    def test_unowned_ignored_remains_orphaned_behind_valid_runner(self):
        files = {
            "crates/my-crate/tests/db_store_unowned.rs": IGNORED_SRC,
            "crates/my-crate/Cargo.toml": MYCRATE_MANIFEST,
        }
        errors = check_fixture(with_direct_markers(VALID_CHECK_YML), NIGHTLY_YML, files)
        assert_orphaned(self, errors, "db_store_unowned", True)

    def test_future_ignored_unowned_remains_orphaned(self):
        files = {
            "crates/my-crate/tests/future_ignored_new.rs": IGNORED_SRC,
            "crates/my-crate/Cargo.toml": MYCRATE_MANIFEST,
        }
        errors = check_fixture(with_direct_markers(VALID_CHECK_YML), NIGHTLY_YML, files)
        assert_orphaned(self, errors, "future_ignored_new", True)

    def test_foreign_package_alive_is_rejected(self):
        assert_valid_dag(self, parse_jobs(VALID_CHECK_YML))
        errors = check_fixture(with_direct_markers(VALID_CHECK_YML), NIGHTLY_YML, dict(OTHER_ALIVE))
        assert_orphaned(self, errors, "alive", True)

    def test_runner_without_cargo_metadata_does_not_cover_unignored(self):
        broken = break_metadata_call(REAL_RUNNER_TEXT)
        self.assertNotIn('"cargo", "metadata"', broken)
        errors = check_fixture(
            with_direct_markers(VALID_CHECK_YML), NIGHTLY_YML, dict(MYCRATE_PLAIN), runner_text=broken)
        assert_orphaned(self, errors, "plain", False)

    def test_metadata_comment_and_unused_literal_do_not_route(self):
        broken = with_metadata_decoys(break_metadata_call(REAL_RUNNER_TEXT))
        self.assertIn("cargo metadata", broken)
        self.assertNotIn('"cargo", "metadata"', broken)
        errors = check_fixture(
            with_direct_markers(VALID_CHECK_YML), NIGHTLY_YML, dict(MYCRATE_PLAIN), runner_text=broken)
        assert_orphaned(self, errors, "plain", False)

    def test_empty_discovery_return_does_not_route(self):
        broken = REAL_RUNNER_TEXT.replace(
            "def integration_commands(index):\n",
            "def integration_commands(index):\n    return []\n", 1)
        self.assertIn('"cargo", "metadata"', broken)
        self.assertIn("return []", broken)
        errors = check_fixture(
            with_direct_markers(VALID_CHECK_YML), NIGHTLY_YML, dict(MYCRATE_PLAIN), runner_text=broken)
        assert_orphaned(self, errors, "plain", False)

    def test_removed_dispatch_does_not_route(self):
        broken = REAL_RUNNER_TEXT.replace(
            "return integration_commands(int(partition[-1]))", "return []", 1)
        self.assertIn('"cargo", "metadata"', broken)
        self.assertNotIn("return integration_commands(int(partition[-1]))", broken)
        errors = check_fixture(
            with_direct_markers(VALID_CHECK_YML), NIGHTLY_YML, dict(MYCRATE_PLAIN), runner_text=broken)
        assert_orphaned(self, errors, "plain", False)

    def test_runner_without_owned_name_does_not_cover_ignored(self):
        broken = break_alive_owner(REAL_RUNNER_TEXT)
        self.assertNotIn('"alive"', broken)
        errors = check_fixture(
            with_direct_markers(VALID_CHECK_YML), NIGHTLY_YML, dict(BOT_ALIVE), runner_text=broken)
        assert_orphaned(self, errors, "alive", True)

    def test_alive_comment_and_unused_literal_do_not_route(self):
        broken = with_alive_decoys(break_alive_owner(REAL_RUNNER_TEXT))
        self.assertIn("alive", broken)
        self.assertNotIn('"alive"', broken.replace('decoy_alive = "alive"', ""))
        errors = check_fixture(
            with_direct_markers(VALID_CHECK_YML), NIGHTLY_YML, dict(BOT_ALIVE), runner_text=broken)
        assert_orphaned(self, errors, "alive", True)

    def test_reasoned_exemption_still_covers_unowned_runner_target(self):
        real_exempt = gate.EXEMPT
        gate.EXEMPT = {**real_exempt, "db_store_unowned": "fixture reason"}
        try:
            files = {
                "crates/my-crate/tests/db_store_unowned.rs": IGNORED_SRC,
                "crates/my-crate/Cargo.toml": MYCRATE_MANIFEST,
            }
            errors = check_fixture(with_direct_markers(VALID_CHECK_YML), NIGHTLY_YML, files)
        finally:
            gate.EXEMPT = real_exempt
        self.assertEqual(errors, [])


def insert_at_function_start(runner_text, function, snippet):
    lines = runner_text.splitlines(keepends=True)
    for index, line in enumerate(lines):
        if line.startswith(f"def {function}("):
            return "".join(lines[:index + 1]) + snippet + "".join(lines[index + 1:])
    raise AssertionError(f"runner fixture lost def {function}()")


class ConditionalEarlyExitRejectedTests(unittest.TestCase):
    """A nested unconditional exit before Cargo must earn no runner credit.

    The scanner already rejects a bare top-level ``return`` in ``execute``.
    The same exit wrapped in a constant condition, a loop or a ``try``
    block executes just as surely before any Cargo command runs, so it
    must lose the same discovery, unit/doc and ignored-target credit.
    These controls exercise the real scanner on a rewritten runner; they
    do not count source substrings in the fixture.
    """

    def assert_no_runner_credit(self, errors):
        assert_orphaned(self, errors, "plain", False)
        assert_orphaned(self, errors, "alive", True)
        self.assertTrue(
            any("workspace lib/bins unit run" in error for error in errors),
            f"early exit kept the lib/bins marker: {errors}")
        self.assertTrue(
            any("workspace doc-test run" in error for error in errors),
            f"early exit kept the doc marker: {errors}")

    def check_rewritten_runner(self, runner_text):
        files = dict(MYCRATE_PLAIN) | dict(BOT_ALIVE)
        return check_fixture(
            VALID_CHECK_YML, NIGHTLY_YML, files, runner_text=runner_text)

    def test_intact_runner_still_routes_mixed_tree(self):
        self.assertEqual(self.check_rewritten_runner(REAL_RUNNER_TEXT), [])

    def test_direct_early_return_in_execute_is_rejected(self):
        broken = insert_at_function_start(
            REAL_RUNNER_TEXT, "execute", "    return 0.0\n")
        self.assert_no_runner_credit(self.check_rewritten_runner(broken))

    def test_conditional_early_exits_in_execute_do_not_route(self):
        exits = {
            "if_true": "    if True:\n        return 0.0\n",
            "if_true_deeply_nested": (
                "    if True:\n        if True:\n            return 0.0\n"),
            "while_true": "    while True:\n        return 0.0\n",
            "try_finally": (
                "    try:\n        return 0.0\n"
                "    finally:\n        pass\n"),
            "if_true_raise": "    if True:\n        raise SystemExit(0)\n",
        }
        for kind, snippet in exits.items():
            with self.subTest(kind=kind):
                broken = insert_at_function_start(
                    REAL_RUNNER_TEXT, "execute", snippet)
                self.assert_no_runner_credit(
                    self.check_rewritten_runner(broken))

    def test_conditional_early_return_in_main_does_not_route(self):
        # main() legitimately returns from its --list/--verify-receipts
        # branches; an unconditional return ahead of lane dispatch is a
        # different statement and must not hide behind those branches.
        broken = insert_at_function_start(
            REAL_RUNNER_TEXT, "main", "    if True:\n        return\n")
        self.assert_no_runner_credit(self.check_rewritten_runner(broken))

    def test_conditional_early_return_in_discovery_does_not_route(self):
        # Insert after the metadata call: an exit at the very first
        # statement is already caught by the scanner's first-statement
        # pin, while one here still discards every discovered target.
        anchor = "    graph = json.loads(proc.stdout)\n"
        self.assertIn(anchor, REAL_RUNNER_TEXT)
        exits = {
            "if_true": "    if True:\n        return []\n",
            "while_true": "    while True:\n        return []\n",
        }
        for kind, snippet in exits.items():
            with self.subTest(kind=kind):
                broken = REAL_RUNNER_TEXT.replace(anchor, snippet + anchor, 1)
                self.assertIn('"cargo", "metadata"', broken)
                errors = check_fixture(
                    with_direct_markers(VALID_CHECK_YML), NIGHTLY_YML,
                    dict(MYCRATE_PLAIN), runner_text=broken)
                assert_orphaned(self, errors, "plain", False)


class CompileOnlyIgnoredTargetRejectedTests(unittest.TestCase):
    """Building alive is preparation, never ignored-test execution ownership."""

    PREP = "cargo test -p two-bot --locked --test alive --no-run"
    EXECUTE = "cargo test -p two-bot --locked --test alive -- --ignored --nocapture"
    COMPILE_COMMANDS = (
        PREP,
        "cargo test --no-run -p two-bot --locked --test alive",
        "cargo test -p two-bot --no-run --test alive --locked",
        "cargo test --locked --test=alive -p two-bot --no-run",
    )

    def direct_workflow(self, command):
        return with_direct_markers(
            "jobs:\n  check:\n    steps:\n" + ALL_TARGETS_STEP + FUZZ_STEP
            + "      - run: " + command + "\n")

    def connected_workflow(self, worker_run=None):
        before, worker = VALID_CHECK_YML.split(
            "  ignored-db-runtime-partition:\n", 1)
        worker, after = worker.split("  ignored-db-runtime:\n", 1)
        prep = (
            "      - name: Compile cold runtime smoke\n"
            "        if: matrix.partition == 'other'\n"
            "        timeout-minutes: 10\n"
            "        run: " + self.PREP + "\n")
        worker = worker.replace(
            "      - name: Run selected test partition\n",
            prep + "      - name: Run selected test partition\n", 1)
        if worker_run is not None:
            worker = worker.replace(
                'run: python3 scripts/ci/run-test-partition.py --lane "$CI_TEST_LANE" --partition "$CI_TEST_PARTITION"',
                worker_run, 1)
        return (before + "  ignored-db-runtime-partition:\n" + worker
                + "  ignored-db-runtime:\n" + after)

    def test_compile_only_alive_does_not_earn_direct_execution_credit(self):
        for command in self.COMPILE_COMMANDS:
            with self.subTest(command=command):
                routed, _ = gate.routed_targets(self.direct_workflow(command))
                self.assertNotIn(
                    "alive", routed,
                    "compile-only Cargo invocation earned execution credit")

    def test_compile_only_alive_remains_an_orphaned_ignored_target(self):
        for command in self.COMPILE_COMMANDS:
            with self.subTest(command=command):
                errors = check_fixture(
                    self.direct_workflow(command), NIGHTLY_YML,
                    dict(BOT_ALIVE), runner_text=None)
                assert_orphaned(self, errors, "alive", True)

    def test_real_alive_execution_still_routes_with_or_without_preparation(self):
        commands = (
            self.EXECUTE,
            self.PREP + "\n      - run: " + self.EXECUTE,
            self.PREP + " && " + self.EXECUTE,
            self.EXECUTE + " --skip compile_only_--no-run_decoy",
            self.EXECUTE + " # --no-run is only a comment",
        )
        for command in commands:
            with self.subTest(command=command):
                check_yml = self.direct_workflow(command)
                routed, _ = gate.routed_targets(check_yml)
                self.assertIn("alive", routed)
                self.assertEqual(check_fixture(
                    check_yml, NIGHTLY_YML, dict(BOT_ALIVE),
                    runner_text=None), [])

    def test_compile_preparation_preserves_intact_connected_runner_credit(self):
        check_yml = self.connected_workflow()
        assert_valid_dag(self, parse_jobs(check_yml))
        self.assertEqual(check_fixture(
            check_yml, NIGHTLY_YML, dict(BOT_ALIVE)), [])

    def test_compile_preparation_cannot_rescue_an_unexecuted_partition(self):
        connected = self.connected_workflow()
        cases = {
            "worker exits before runner": (
                self.connected_workflow(
                    'run: |\n          exit 0\n          python3 scripts/ci/run-test-partition.py --lane "$CI_TEST_LANE" --partition "$CI_TEST_PARTITION"'),
                REAL_RUNNER_TEXT),
            "planner disconnected": (
                connected.replace(
                    'matrix=$(python3 scripts/ci/run-test-partition.py --lane "$lane" --list)',
                    'matrix=$(echo "$lane")'), REAL_RUNNER_TEXT),
            "stable final disconnected": (
                connected.replace(
                    'run-test-partition.py --lane "$CI_TEST_LANE" --verify-receipts test-partition-receipts',
                    'echo verify-receipts'), REAL_RUNNER_TEXT),
            "runner exits before Cargo": (
                connected, insert_at_function_start(
                    REAL_RUNNER_TEXT, "execute", "    if True:\n        return 0.0\n")),
        }
        for kind, (check_yml, runner_text) in cases.items():
            with self.subTest(kind=kind):
                errors = check_fixture(
                    with_direct_markers(check_yml), NIGHTLY_YML,
                    dict(BOT_ALIVE), runner_text=runner_text)
                assert_orphaned(self, errors, "alive", True)


class NonfatalEntrypointRejectedTests(unittest.TestCase):
    """A main() call alone cannot prove the runner propagates failure."""

    def with_handler(self, body, tail=""):
        guard = ast.parse(REAL_RUNNER_TEXT).body[-1]
        self.assertIsInstance(guard, ast.If)
        prefix = "".join(REAL_RUNNER_TEXT.splitlines(keepends=True)[:guard.lineno - 1])
        handler = "".join("        " + line + "\n" for line in body.splitlines())
        return prefix + (
            'if __name__ == "__main__":\n'
            '    try:\n'
            '        main()\n'
            '    except (ValueError, KeyError, OSError, subprocess.SubprocessError) as exc:\n'
            '        print(f"partition failed: {exc}", file=sys.stderr)\n'
            + handler + tail)

    def nonfatal_handlers(self):
        return {
            "exit zero": self.with_handler("sys.exit(0)"),
            "no exit": self.with_handler(""),
            "pass": self.with_handler("pass"),
            "exit without status": self.with_handler("sys.exit()"),
            "false status": self.with_handler("sys.exit(False)"),
            "raise zero": self.with_handler("raise SystemExit(0)"),
            "unreachable fatal exit": self.with_handler("if False:\n    sys.exit(1)"),
            "swallowed SystemExit": self.with_handler(
                "try:\n    sys.exit(1)\nexcept SystemExit:\n    pass"),
        }

    def replacing_tails(self):
        return {
            "finally exits zero": self.with_handler(
                "sys.exit(1)", "    finally:\n        sys.exit(0)\n"),
            "finally raises zero": self.with_handler(
                "sys.exit(1)", "    finally:\n        raise SystemExit(0)\n"),
            "fatal else is not an exception exit": self.with_handler(
                "pass", "    else:\n        sys.exit(1)\n"),
        }

    def check_runner(self, runner_text):
        return check_fixture(
            VALID_CHECK_YML, NIGHTLY_YML,
            dict(BOT_ALIVE) | dict(MYCRATE_PLAIN), runner_text=runner_text)

    def assert_no_credit(self, errors):
        assert_orphaned(self, errors, "plain", False)
        assert_orphaned(self, errors, "alive", True)
        for marker in ("workspace lib/bins unit run", "workspace doc-test run"):
            self.assertTrue(any(marker in error for error in errors),
                            f"nonfatal runner kept {marker}: {errors}")

    def run_cli_without_cargo(self, runner_text, *args):
        with tempfile.TemporaryDirectory(prefix="runner-exit-control-") as directory:
            root = Path(directory)
            runner = root / "run-test-partition.py"
            runner.write_text(runner_text)
            binary = root / "bin"
            binary.mkdir()
            marker = root / "unexpected-cargo"
            cargo = binary / "cargo"
            cargo.write_text(f"#!{sys.executable}\nfrom pathlib import Path\n"
                             f"Path({str(marker)!r}).touch()\nraise SystemExit(91)\n")
            cargo.chmod(0o755)
            env = {
                "PATH": str(binary) + os.pathsep + os.defpath,
                "HOME": str(root), "RUNNER_TEMP": str(root),
                "PYTHONPATH": str(REAL_RUNNER.parent),
                "PYTHONDONTWRITEBYTECODE": "1",
            }
            proc = subprocess.run(
                [sys.executable, str(runner), *args], cwd=root, env=env,
                capture_output=True, text=True, timeout=10)
            self.assertFalse(marker.exists(), "receipt/selector control invoked Cargo")
            self.assertFalse((root / "test-partition-receipts").exists(),
                             "receipt/selector control wrote an execution receipt")
            return proc

    def test_intact_handler_keeps_routing_and_rejects_invalid_receipts(self):
        self.assertEqual(self.check_runner(REAL_RUNNER_TEXT), [])
        proc = self.run_cli_without_cargo(
            REAL_RUNNER_TEXT, "--lane", "rust-tests", "--verify-receipts", "missing-receipts")
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("partition failed:", proc.stderr)

    def test_legitimate_list_and_validation_branches_keep_their_exit_status(self):
        expected = {
            "rust-tests": ["unit-bin", "integration-0", "integration-1", "integration-2", "replays"],
            "ignored-db-runtime": ["onboarding", "gateway", "runtime", "cutover", "other"],
        }
        for lane, partitions in expected.items():
            with self.subTest(lane=lane):
                proc = self.run_cli_without_cargo(REAL_RUNNER_TEXT, "--lane", lane, "--list")
                self.assertEqual(proc.returncode, 0, proc.stderr)
                self.assertEqual([row["partition"] for row in json.loads(proc.stdout)["include"]],
                                 partitions)
        for args in (("--lane", "wrong-lane", "--list"), ("--lane", "rust-tests"),
                     ("--lane", "rust-tests", "--partition", "wrong-partition")):
            with self.subTest(args=args):
                proc = self.run_cli_without_cargo(REAL_RUNNER_TEXT, *args)
                self.assertNotEqual(proc.returncode, 0)
                self.assertIn("error:", proc.stderr)

    def test_missing_main_dispatch_is_already_rejected_for_every_credit(self):
        broken = self.with_handler("sys.exit(1)").replace(
            "        main()\n", "        pass\n", 1)
        self.assert_no_credit(self.check_runner(broken))

    def test_nonfatal_exception_handlers_earn_no_runner_credit(self):
        for name, runner_text in self.nonfatal_handlers().items():
            with self.subTest(handler=name):
                self.assert_no_credit(self.check_runner(runner_text))

    def test_replacing_finally_or_else_paths_earn_no_runner_credit(self):
        for name, runner_text in self.replacing_tails().items():
            with self.subTest(tail=name):
                self.assert_no_credit(self.check_runner(runner_text))

    def test_adverse_handlers_really_return_success_for_invalid_receipts(self):
        # Qualify the hostile inputs through bounded native Python execution,
        # not a count of source mentions. These are rewritten copies, never
        # the checked-in production runner; no Cargo/DB operation is selected.
        for name, runner_text in (self.nonfatal_handlers() | self.replacing_tails()).items():
            with self.subTest(handler=name):
                proc = self.run_cli_without_cargo(
                    runner_text, "--lane", "rust-tests", "--verify-receipts", "missing-receipts")
                self.assertEqual(proc.returncode, 0, proc.stderr)
                self.assertIn("partition failed:", proc.stderr)


if __name__ == "__main__":
    unittest.main()
