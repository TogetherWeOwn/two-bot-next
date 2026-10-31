"""Protected acceptance tests: routing evidence must be executable.

The inventory HARD gate (``check-test-inventory.py``) promises in its
docstring that an ignored integration target is covered only when it is
referenced "in a run context". These tests pin that contract at the
behaviour level through ``check(root)`` and the CLI:

- a target named only in display text (step/job ``name:``), an ``env:``
  value, an ``if:`` expression, an inline comment, an ``echo``/``printf``
  line, or a ``--skip`` argument is orphaned when no executable route
  exists;
- genuine execution in the shapes the real workflows use today (direct
  ``--test <name>``, a multi-``--test`` target list on one command, the
  ``--test=<name>`` equals form, a ``cov_run ... llvm-cov ... --test
  <name>`` wrapper, a backslash-continued command, a route that lives
  only in ``nightly.yml``, and an explicit ``for t in <list>`` loop whose
  body runs ``--test "$t"``) stays routed;
- removing the actual command from an otherwise identical fixture flips
  the verdict from pass to orphaned;
- documented exemptions, the blanket workspace run for unignored
  targets, the fuzz checks and the required lib/bins/doc/all-targets
  markers keep working (every fixture below carries them, so the only
  verdict that varies is the ignored target's).

Fixtures build a miniature repository; no Cargo build, network or
workflow dispatch is involved.
"""

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = Path(__file__).with_name("check-test-inventory.py")

import importlib.util

SPEC = importlib.util.spec_from_file_location("inventory_gate_acceptance", SCRIPT)
gate = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(gate)


# Markers every fixture carries so only the ignored target's routing varies:
# workspace lib/bins, doc tests, --all-targets, the blanket --test '*' run
# (covers the unignored `plain` target) and the fuzz build.
REQUIRED_STEPS = """\
      - run: cargo test --workspace --lib --bins --locked
      - run: cargo test --workspace --doc --locked
      - run: cargo clippy --workspace --all-targets --locked
      - run: cargo test --workspace --test '*' --locked
      - run: cargo +nightly-2026-10-01 fuzz build
"""

DIRECT_ROUTE = "      - run: cargo test -p my-crate --test db_store --locked -- --ignored\n"


def workflows(extra_check_steps="", extra_nightly_steps="", job_env=""):
    env_block = f"    env:\n{job_env}" if job_env else ""
    check_yml = (
        "jobs:\n  check:\n"
        + env_block
        + "    steps:\n"
        + REQUIRED_STEPS
        + extra_check_steps
    )
    nightly_yml = (
        "jobs:\n  sweep:\n    steps:\n"
        "      - run: cargo test --workspace --locked\n"
        + extra_nightly_steps
    )
    return check_yml, nightly_yml


def make_root(check_yml, nightly_yml):
    tmp = tempfile.TemporaryDirectory()
    root = Path(tmp.name)
    files = {
        ".github/workflows/check.yml": check_yml,
        ".github/workflows/nightly.yml": nightly_yml,
        "fuzz/Cargo.toml": '[[bin]]\nname = "wiggle"\npath = "fuzz_targets/wiggle.rs"\n',
        "fuzz/fuzz_targets/wiggle.rs": "// target\n",
        "crates/my-crate/tests/plain.rs": "#[test]\nfn it() {}\n",
        "crates/my-crate/tests/db_store.rs": "#[ignore]\n#[test]\nfn it() {}\n",
    }
    for path, content in files.items():
        full = root / path
        full.parent.mkdir(parents=True, exist_ok=True)
        full.write_text(content)
    return tmp, root


def check_fixture(check_yml, nightly_yml):
    tmp, root = make_root(check_yml, nightly_yml)
    try:
        return gate.check(root)
    finally:
        tmp.cleanup()


class MentionOnlyIsOrphanedTests(unittest.TestCase):
    """Display/non-executing mentions must not count as routing."""

    def assert_orphaned(self, check_yml, nightly_yml):
        errors = check_fixture(check_yml, nightly_yml)
        self.assertEqual(
            len(errors), 1, f"expected exactly the db_store orphan, got: {errors}")
        self.assertIn("db_store", errors[0])
        self.assertIn("orphaned", errors[0])

    def test_step_display_name_only_is_orphaned(self):
        check_yml, nightly_yml = workflows(
            "      - name: Run db_store ignored suite\n"
            "        run: true\n")
        self.assert_orphaned(check_yml, nightly_yml)

    def test_job_display_name_only_is_orphaned(self):
        check_yml, nightly_yml = workflows()
        check_yml = check_yml.replace(
            "  check:\n", "  check:\n    name: db_store lane\n")
        self.assert_orphaned(check_yml, nightly_yml)

    def test_env_value_only_is_orphaned(self):
        check_yml, nightly_yml = workflows(
            job_env="      IGNORED_TARGET: db_store\n")
        self.assert_orphaned(check_yml, nightly_yml)

    def test_step_env_value_only_is_orphaned(self):
        check_yml, nightly_yml = workflows(
            "      - run: true\n"
            "        env:\n"
            "          TARGET: db_store\n")
        self.assert_orphaned(check_yml, nightly_yml)

    def test_if_expression_only_is_orphaned(self):
        check_yml, nightly_yml = workflows(
            "      - if: ${{ matrix.target == 'db_store' }}\n"
            "        run: true\n")
        self.assert_orphaned(check_yml, nightly_yml)

    def test_inline_trailing_comment_only_is_orphaned(self):
        check_yml, nightly_yml = workflows(
            "      - run: true # db_store runs in this lane\n")
        self.assert_orphaned(check_yml, nightly_yml)

    def test_echo_only_is_orphaned(self):
        check_yml, nightly_yml = workflows(
            '      - run: echo "db_store would run here"\n')
        self.assert_orphaned(check_yml, nightly_yml)

    def test_echo_in_multiline_run_block_only_is_orphaned(self):
        check_yml, nightly_yml = workflows(
            "      - run: |\n"
            "          echo starting lanes\n"
            "          printf '%s\\n' db_store\n")
        self.assert_orphaned(check_yml, nightly_yml)

    def test_skip_equals_argument_only_is_orphaned(self):
        check_yml, nightly_yml = workflows(
            extra_nightly_steps=(
                "      - run: cargo test --workspace --locked "
                "-- --include-ignored --skip=db_store\n"))
        self.assert_orphaned(check_yml, nightly_yml)

    def test_loop_list_without_test_execution_is_orphaned(self):
        # The name sits in an explicit list inside a run block, but the loop
        # body never executes a test command: list membership alone is a
        # mention, not a route.
        check_yml, nightly_yml = workflows(
            "      - run: |\n"
            "          for t in db_store other_store; do\n"
            '            echo "lane for $t"\n'
            "          done\n")
        self.assert_orphaned(check_yml, nightly_yml)

    def test_removing_the_command_flips_pass_to_orphaned(self):
        # Same fixture, same display name: only the executable command is
        # removed, so the verdict must flip.
        named_step = "      - name: Run db_store ignored suite\n"
        routed_check, nightly_yml = workflows(
            named_step + "        run: cargo test -p my-crate --test db_store --locked -- --ignored\n")
        self.assertEqual(check_fixture(routed_check, nightly_yml), [])

        unrouted_check, _ = workflows(named_step + "        run: true\n")
        self.assert_orphaned(unrouted_check, nightly_yml)


class ExecutableRouteIsRecognizedTests(unittest.TestCase):
    """The command shapes the real workflows use must stay routed."""

    def assert_routed(self, check_yml, nightly_yml):
        self.assertEqual(check_fixture(check_yml, nightly_yml), [])

    def test_direct_test_command_is_routed(self):
        check_yml, nightly_yml = workflows(DIRECT_ROUTE)
        self.assert_routed(check_yml, nightly_yml)

    def test_multi_target_list_on_one_command_is_routed(self):
        check_yml, nightly_yml = workflows(
            "      - run: cargo test -p my-crate --test audit_store "
            "--test db_store --test join_risk_store --locked -- --ignored\n")
        self.assert_routed(check_yml, nightly_yml)

    def test_test_equals_form_is_routed(self):
        check_yml, nightly_yml = workflows(
            "      - run: cargo test -p my-crate --test=db_store --locked -- --ignored\n")
        self.assert_routed(check_yml, nightly_yml)

    def test_cov_run_llvm_cov_wrapped_command_is_routed(self):
        # Coverage lanes wrap the test command: the executable evidence is
        # the --test <name> argument, not a leading `cargo test`.
        check_yml, nightly_yml = workflows(
            "      - run: |\n"
            '          cov_run "$COV" llvm-cov --no-report -p my-crate '
            "--locked --test db_store -- --ignored --test-threads=1\n")
        self.assert_routed(check_yml, nightly_yml)

    def test_backslash_continued_command_is_routed(self):
        check_yml, nightly_yml = workflows(
            "      - run: |\n"
            "          cargo test -p my-crate --locked \\\n"
            "            --test db_store -- --ignored\n")
        self.assert_routed(check_yml, nightly_yml)

    def test_route_only_in_nightly_is_routed(self):
        check_yml, nightly_yml = workflows(
            extra_nightly_steps=DIRECT_ROUTE)
        self.assert_routed(check_yml, nightly_yml)

    def test_explicit_target_list_loop_is_routed(self):
        check_yml, nightly_yml = workflows(
            "      - run: |\n"
            "          for t in db_store other_store; do\n"
            '            cargo test -p my-crate --test "$t" --locked -- --ignored\n'
            "          done\n")
        self.assert_routed(check_yml, nightly_yml)


class PreservedGateBehaviourTests(unittest.TestCase):
    def test_reasoned_exemption_still_covers_a_mention_only_target(self):
        check_yml, nightly_yml = workflows(
            "      - name: Run db_store ignored suite\n"
            "        run: true\n")
        real_exempt = gate.EXEMPT
        gate.EXEMPT = {**real_exempt, "db_store": "fixture reason"}
        try:
            errors = check_fixture(check_yml, nightly_yml)
        finally:
            gate.EXEMPT = real_exempt
        self.assertEqual(errors, [])

    def test_real_tree_still_passes(self):
        # Every route the real check.yml/nightly.yml use today must remain
        # recognized; tightening mention-only detection must not orphan a
        # single real target.
        self.assertEqual(gate.check(ROOT), [])


class CliTests(unittest.TestCase):
    def run_cli(self, check_yml, nightly_yml):
        tmp, root = make_root(check_yml, nightly_yml)
        try:
            return subprocess.run(
                [sys.executable, str(SCRIPT), "--root", str(root)],
                capture_output=True, text=True, timeout=60)
        finally:
            tmp.cleanup()

    def test_cli_fails_and_names_the_mention_only_target(self):
        check_yml, nightly_yml = workflows(
            "      - name: Run db_store ignored suite\n"
            "        run: true\n")
        result = self.run_cli(check_yml, nightly_yml)
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("db_store", result.stderr)

    def test_cli_passes_a_routed_tree(self):
        check_yml, nightly_yml = workflows(DIRECT_ROUTE)
        result = self.run_cli(check_yml, nightly_yml)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("every target is routed", result.stdout)


if __name__ == "__main__":
    unittest.main()
