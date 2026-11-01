"""Protected acceptance tests: reject soak commands that execute no selected tests.

Acceptance-owned: preserve this file in the combined implementation change;
return suspected test defects to its author, rather than weakening assertions.
This file is intentionally RED until the implementation satisfies the contract.

Contract (offline, stdlib only, no Cargo, subprocesses or network):

* Refuse Cargo ``--no-run`` and libtest ``--list`` as automated soak coverage.
  The validator uses an explicit bounded supported-option vocabulary and
  enforces value arity, including after ``--``.
* Enforce Cargo's single pre-separator filter: at most one positional
  test-name filter may appear before ``--`` (pinned Cargo 1.98.1 rejects a
  second with unexpected-argument and runs nothing); additional filters
  belong after ``--`` where libtest accepts several. A second
  pre-separator filter fails with ``UNSUPPORTED``.
* Compute nonempty effective selected tests after positive filters, ``--exact``,
  ``--skip`` and ignore-mode filtering. Default excludes ``#[ignore]`` tests,
  ``--ignored`` selects only ignored tests, ``--include-ignored`` selects all
  matched tests; the scanner preserves ignored identity across inline and
  external modules. A selected test that is entirely skipped or excluded by
  its ignore mode cannot satisfy coverage; a supported skip leaving real
  executable tests passes. Fail closed for unsupported semantics (misplaced
  known options, excess pre-separator filters).
* Unknown flags and missing/invalid option values fail with stable row-scoped
  reason codes and a next action, without invoking Cargo, subprocesses or
  network.
* Keep the checklist and this validator as the single source of truth. The
  validator documents compact supported-command examples where agents look
  and an offline regression command. Valid existing rows keep resolving, and
  static executable selection stays distinct from actual run evidence.

Reason codes (stable, uppercase tokens in the message):

* ``NOEXEC``: Cargo ``--no-run``, libtest ``--list`` and other non-executing
  modes used as automated coverage.
* ``EMPTY_SELECTION``: no effective tests remain after positive filters,
  ``--exact``, ``--skip`` and ignore-mode filtering (default excludes
  ``#[ignore]``, ``--ignored`` selects only ignored, ``--include-ignored``
  selects all matched).
* ``UNKNOWN_OPTION``: a flag outside the bounded supported vocabulary.
* ``MISSING_VALUE``: a value option with no value token (``--skip`` or
  ``--test-threads`` at the end, ``--test`` or ``-p`` without a value).
* ``INVALID_VALUE``: a value option with an empty or malformed value
  (``--test-threads`` not a positive integer, ``--skip=`` empty, or a flag
  given ``=value`` such as ``--exact=foo``, ``--ignored=foo``, ``--lib=foo``).
* ``UNSUPPORTED``: a known option in the wrong position (libtest ``--skip`` /
  ``--exact`` / ``--ignored`` before ``--``; Cargo ``-p`` / ``--test`` /
  ``--features`` after ``--``) or a second positional test-name filter
  before ``--`` (Cargo accepts at most one; pass the rest after ``--``).

Every new failure message is row-scoped (starts with ``"<entry_id>: "``),
contains its reason code verbatim, and contains a next action (the message,
lowercased, contains ``"next:"`` followed by guidance). The same invalid
command always produces the same message.

Isolation: every ``check_verification`` call in this file runs with
``subprocess``, ``os.system``/``os.popen`` and ``socket`` blocked. A fix that
shells out to Cargo (for example ``cargo test --list``) to compute selection
fails these tests. No fixture invokes Cargo, the network or a database.

Run: PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_soak_checklist_noexec_acceptance.py' -v
"""

import copy
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import textwrap
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import check_soak_checklist as checker
from check_soak_checklist import check_verification, render, validate

ROOT = Path(__file__).resolve().parents[1]
WRAPPED = "python3 scripts/cargo_cache.py run -- test "

# Fixture libtest names: tests::alpha_test, tests::beta_test,
# tests::gamma_other (nonignored inline); tests::quarantined_inline
# (ignored inline); outer::quarantined_outer_test (ignored external via
# `mod outer;`); smoke_case (nonignored integration);
# quarantined_smoke (ignored integration). Ignored names avoid the
# alpha/beta/gamma/smoke substrings used by the nonignored controls, and
# the external ignored name keeps a "test" substring so the whole-target
# `--skip test` control still skips every lib test.
FIXTURE = {
    "Cargo.toml": """
        [workspace]
        members = ["crates/demo"]
    """,
    "crates/demo/Cargo.toml": """
        [package]
        name = "demo"
    """,
    "crates/demo/src/lib.rs": """
        mod outer;
        #[cfg(test)]
        mod tests {
            #[test]
            fn alpha_test() {}
            #[test]
            fn beta_test() {}
            #[test]
            fn gamma_other() {}
            #[ignore]
            #[test]
            fn quarantined_inline() {}
        }
    """,
    "crates/demo/src/outer.rs": """
        #[ignore]
        #[test]
        fn quarantined_outer_test() {}
    """,
    "crates/demo/tests/smoke.rs": """
        #[test] fn smoke_case() {}
        #[ignore]
        #[test] fn quarantined_smoke() {}
    """,
}

# Every subprocess / shell / network entry the validator must never touch.
# The implementation computes selection from source text only.
BLOCKED_CALLS = (
    "subprocess.run",
    "subprocess.Popen",
    "subprocess.call",
    "subprocess.check_output",
    "subprocess.check_call",
    "os.system",
    "os.popen",
    "os.spawnl",
    "os.spawnlp",
    "os.execv",
    "os.execvp",
    "socket.socket",
    "socket.create_connection",
)


def blocked_call(*args, **kwargs):
    raise AssertionError("validator must not invoke subprocesses or network")


def checked(command, entry_id="row", root=None):
    """Run check_verification with subprocesses and network blocked."""
    patches = [mock.patch(target, side_effect=blocked_call) for target in BLOCKED_CALLS]
    for entered in patches:
        entered.start()
    try:
        if root is None:
            return check_verification(entry_id, command)
        return check_verification(entry_id, command, root)
    finally:
        for entered in patches:
            entered.stop()


class NoexecAcceptanceBase(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        scratch = (os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")
                   or os.environ.get("PAPERCLIP_SCRATCH_DIR"))
        cls.fixture = tempfile.TemporaryDirectory(dir=scratch)
        cls.root = Path(cls.fixture.name)
        for name, source in FIXTURE.items():
            (cls.root / name).parent.mkdir(parents=True, exist_ok=True)
            (cls.root / name).write_text(textwrap.dedent(source))
        cls.parity = (ROOT / "docs/parity.md").read_text()
        cls.checklist = json.loads((ROOT / "docs/soak-checklist.json").read_text())

    @classmethod
    def tearDownClass(cls):
        cls.fixture.cleanup()

    def assert_rejects(self, command, code, entry_id="row"):
        with self.assertRaises(ValueError) as caught:
            checked(WRAPPED + command, entry_id, self.root)
        message = str(caught.exception)
        self.assertTrue(
            message.startswith(f"{entry_id}: "),
            f"row-scoped message must start with '{entry_id}: ': {message[:160]}")
        self.assertIn(code, message, f"stable reason code {code} missing: {message[:300]}")
        self.assertIn(
            "next:", message.lower(),
            f"next action ('next: ...') missing: {message[:300]}")
        return message

    def assert_accepts(self, command, entry_id="row"):
        try:
            checked(WRAPPED + command, entry_id, self.root)
        except ValueError as error:
            self.fail(f"valid command must keep resolving: {command} -> {error}")

    def assert_stable(self, command, code, entry_id="row"):
        first = self.assert_rejects(command, code, entry_id)
        second = self.assert_rejects(command, code, entry_id)
        self.assertEqual(first, second, "same invalid command must produce the same message")


class NonExecutingModesTests(NoexecAcceptanceBase):
    """Cargo --no-run and libtest --list cannot satisfy automated coverage."""

    def test_cargo_no_run_is_refused_before_filters(self):
        for command in (
            "-p demo --lib alpha_test --no-run",
            "--no-run -p demo --lib alpha_test",
            "-p demo --no-run --lib alpha_test",
            "-p demo --test smoke smoke_case --no-run",
        ):
            with self.subTest(command=command):
                self.assert_stable(command, "NOEXEC")

    def test_libtest_list_is_refused_after_separator(self):
        for command in (
            "-p demo --lib -- alpha_test --list",
            "-p demo --lib -- --list alpha_test",
            "-p demo --test smoke smoke_case -- --exact --list",
            "-p demo --lib -- --list",
        ):
            with self.subTest(command=command):
                self.assert_stable(command, "NOEXEC")

    def test_noexec_refusal_is_row_scoped_with_next_action(self):
        message = self.assert_rejects(
            "-p demo --lib alpha_test --no-run", "NOEXEC", entry_id="s9-01")
        self.assertTrue(message.startswith("s9-01: "))
        message = self.assert_rejects(
            "-p demo --lib -- alpha_test --list", "NOEXEC", entry_id="s9-02")
        self.assertTrue(message.startswith("s9-02: "))


class EffectiveSelectionTests(NoexecAcceptanceBase):
    """Effective selection after filters, --exact and --skip must stay nonempty."""

    def test_selected_and_skipped_exact_test_cannot_satisfy_coverage(self):
        self.assert_stable(
            "-p demo --test smoke smoke_case -- --exact --skip smoke_case",
            "EMPTY_SELECTION")

    def test_selected_and_skipped_substring_cannot_satisfy_coverage(self):
        self.assert_stable(
            "-p demo --lib alpha_test -- --skip alpha",
            "EMPTY_SELECTION")

    def test_skip_covering_every_positive_match_fails(self):
        # "test" matches every lib test (via "tests::" for the inline module
        # and the fn name for the external ignored test); skipping "test"
        # leaves nothing executable behind.
        self.assert_stable("-p demo --lib test -- --skip test", "EMPTY_SELECTION")

    def test_skip_covering_every_test_without_positive_filters_fails(self):
        # No positive filter selects the whole target; skipping all of it
        # still leaves no executable test behind.
        self.assert_stable("-p demo --lib -- --skip test", "EMPTY_SELECTION")

    def test_empty_selection_is_row_scoped_with_next_action(self):
        message = self.assert_rejects(
            "-p demo --test smoke smoke_case -- --exact --skip smoke_case",
            "EMPTY_SELECTION", entry_id="s9-03")
        self.assertTrue(message.startswith("s9-03: "))

    def test_supported_skip_leaving_real_tests_passes(self):
        # "test" matches every lib test; default excludes the two ignored and
        # skipping only alpha leaves beta/gamma executable.
        self.assert_accepts("-p demo --lib test -- --skip alpha_test")
        # Cargo accepts at most one filter before --; the two-filter selection
        # with a skip belongs after the separator and still leaves beta.
        self.assert_accepts("-p demo --lib -- alpha_test beta_test --skip alpha")
        # Exact skip that does not exactly match removes nothing.
        self.assert_accepts("-p demo --test smoke smoke_case -- --exact --skip smoke")
        # Skip with a valid thread count still resolves.
        self.assert_accepts("-p demo --lib test -- --skip alpha_test --test-threads=2")


class UnknownOptionTests(NoexecAcceptanceBase):
    """Flags outside the bounded vocabulary fail closed."""

    def test_unknown_cargo_flags_fail(self):
        for command in (
            "-p demo --lib --not-a-cargo-flag alpha",
            "-p demo --lib --unknown=value alpha",
            "-p demo --lib -Z alpha",
        ):
            with self.subTest(command=command):
                self.assert_stable(command, "UNKNOWN_OPTION")

    def test_unknown_libtest_flags_fail(self):
        for command in (
            "-p demo --lib -- alpha --not-a-libtest-flag",
            "-p demo --lib -- alpha --unknown=value",
            "-p demo --test smoke -- smoke_case --bogus",
        ):
            with self.subTest(command=command):
                self.assert_stable(command, "UNKNOWN_OPTION")

    def test_unknown_option_is_row_scoped_with_next_action(self):
        message = self.assert_rejects(
            "-p demo --lib --not-a-cargo-flag alpha", "UNKNOWN_OPTION",
            entry_id="s9-04")
        self.assertTrue(message.startswith("s9-04: "))


class ValueArityTests(NoexecAcceptanceBase):
    """Value options enforce arity, including after --."""

    def test_missing_values_fail(self):
        for command in (
            "-p demo --lib -- alpha --test-threads",
            "-p demo --lib -- alpha --skip",
            "-p demo --test",
            "-p",
            "-p demo --features",
        ):
            with self.subTest(command=command):
                self.assert_stable(command, "MISSING_VALUE")

    def test_invalid_thread_counts_fail(self):
        for command in (
            "-p demo --lib -- alpha --test-threads foo",
            "-p demo --lib -- alpha --test-threads 0",
            "-p demo --lib -- alpha --test-threads=-1",
            "-p demo --lib -- alpha --test-threads=",
            "-p demo --lib -- alpha --test-threads 1.5",
        ):
            with self.subTest(command=command):
                self.assert_stable(command, "INVALID_VALUE")

    def test_empty_skip_value_and_flags_with_values_fail(self):
        for command in (
            "-p demo --lib -- alpha --skip=",
            "-p demo --lib -- alpha --exact=foo",
            "-p demo --lib -- alpha --ignored=foo",
            "-p demo --lib -- alpha --include-ignored=yes",
            "-p demo --lib=foo alpha",
            "-p= alpha",
        ):
            with self.subTest(command=command):
                self.assert_stable(command, "INVALID_VALUE")

    def test_value_errors_are_row_scoped_with_next_action(self):
        message = self.assert_rejects(
            "-p demo --lib -- alpha --test-threads", "MISSING_VALUE",
            entry_id="s9-05")
        self.assertTrue(message.startswith("s9-05: "))
        message = self.assert_rejects(
            "-p demo --lib -- alpha --test-threads foo", "INVALID_VALUE",
            entry_id="s9-06")
        self.assertTrue(message.startswith("s9-06: "))

    def test_valid_values_keep_resolving(self):
        self.assert_accepts("-p demo --lib -- alpha --test-threads 2")
        self.assert_accepts("-p demo --lib -- alpha --test-threads=1")
        self.assert_accepts("-p demo --features db --lib alpha")
        self.assert_accepts("-p demo --test smoke smoke_case -- --exact")


class UnsupportedSemanticsTests(NoexecAcceptanceBase):
    """Known options in the wrong position fail closed."""

    def test_libtest_options_before_separator_fail(self):
        for command in (
            "-p demo --skip foo --lib alpha",
            "-p demo --exact --lib alpha",
            "-p demo --ignored --lib alpha",
            "-p demo --test-threads 2 --lib alpha",
        ):
            with self.subTest(command=command):
                self.assert_stable(command, "UNSUPPORTED")

    def test_cargo_options_after_separator_fail(self):
        for command in (
            "-p demo --lib -- alpha -p demo",
            "-p demo --lib -- alpha --test foo",
            "-p demo --lib -- alpha --features db",
            "-p demo --lib -- alpha --package demo",
        ):
            with self.subTest(command=command):
                self.assert_stable(command, "UNSUPPORTED")

    def test_unsupported_is_row_scoped_with_next_action(self):
        message = self.assert_rejects(
            "-p demo --skip foo --lib alpha", "UNSUPPORTED", entry_id="s9-07")
        self.assertTrue(message.startswith("s9-07: "))


class CargoFilterArityTests(NoexecAcceptanceBase):
    """Cargo accepts at most one positional filter before `--`."""

    def test_second_pre_separator_filter_is_unsupported(self):
        # Pinned Cargo 1.98.1 rejects the second pre-separator filter with
        # unexpected-argument and runs nothing; the validator must fail
        # closed with UNSUPPORTED rather than union both filters.
        for command in (
            "-p demo --lib alpha_test beta_test",
            "-p demo --lib beta_test gamma_other",
            "-p demo --lib alpha_test beta_test -- --skip alpha",
            "-p demo --lib alpha_test beta_test gamma_other",
            "-p demo --test smoke smoke_case quarantined_smoke",
            "-p demo --features db --lib alpha_test beta_test",
        ):
            with self.subTest(command=command):
                self.assert_stable(command, "UNSUPPORTED")

    def test_second_filter_unsupported_is_row_scoped_with_next_action(self):
        message = self.assert_rejects(
            "-p demo --lib alpha_test beta_test", "UNSUPPORTED",
            entry_id="s9-08")
        self.assertTrue(message.startswith("s9-08: "))
        message = self.assert_rejects(
            "-p demo --lib beta_test gamma_other -- --skip beta",
            "UNSUPPORTED", entry_id="s9-09")
        self.assertTrue(message.startswith("s9-09: "))

    def test_valid_filter_arity_resolves(self):
        # One filter before -- is valid Cargo; several filters after -- are
        # valid libtest. Both shapes keep resolving.
        self.assert_accepts("-p demo --lib alpha_test")
        self.assert_accepts("-p demo --lib -- alpha_test beta_test")
        self.assert_accepts("-p demo --lib -- beta_test gamma_other --skip beta")
        self.assert_accepts("-p demo --lib -- alpha_test beta_test gamma_other")
        self.assert_accepts(
            "-p demo --test smoke -- smoke_case quarantined_smoke --include-ignored")


class LibtestOptionArityTests(NoexecAcceptanceBase):
    """Refuse options pinned libtest rejects before executing any tests."""

    # Every filter resolves under either last-wins or union ignore semantics,
    # so EMPTY_SELECTION cannot conceal an accepted invalid option combination.
    CONFLICTING_IGNORE_MODES = (
        "-p demo --lib -- --ignored --include-ignored quarantined_inline",
        "-p demo --lib -- --include-ignored --ignored quarantined_inline",
    )
    REPEATED_EXACT = (
        "-p demo --lib tests::alpha_test -- --exact --exact",
        "-p demo --lib -- --exact tests::alpha_test --exact",
    )
    REPEATED_IGNORED = (
        "-p demo --lib -- --ignored --ignored quarantined_inline",
        "-p demo --lib -- --ignored outer::quarantined_outer_test --ignored",
    )
    REPEATED_INCLUDE_IGNORED = (
        "-p demo --lib -- --include-ignored --include-ignored quarantined_inline",
        "-p demo --lib -- --include-ignored alpha_test --include-ignored",
    )
    REPEATED_TEST_THREADS = (
        "-p demo --lib alpha_test -- --test-threads 1 --test-threads 1",
        "-p demo --lib alpha_test -- --test-threads 1 --test-threads 2",
        "-p demo --lib -- --test-threads=1 alpha_test --test-threads=1",
        "-p demo --lib -- --test-threads=1 alpha_test --test-threads=2",
        "-p demo --test smoke smoke_case -- --test-threads 1 --test-threads=2",
        "-p demo --test smoke -- --test-threads=2 smoke_case --test-threads 1",
    )

    def test_conflicting_ignore_modes_are_unsupported_in_either_order(self):
        for command in self.CONFLICTING_IGNORE_MODES:
            with self.subTest(command=command):
                self.assert_stable(command, "UNSUPPORTED")

    def test_repeated_exact_is_unsupported(self):
        for command in self.REPEATED_EXACT:
            with self.subTest(command=command):
                self.assert_stable(command, "UNSUPPORTED")

    def test_repeated_ignored_is_unsupported(self):
        for command in self.REPEATED_IGNORED:
            with self.subTest(command=command):
                self.assert_stable(command, "UNSUPPORTED")

    def test_repeated_include_ignored_is_unsupported(self):
        for command in self.REPEATED_INCLUDE_IGNORED:
            with self.subTest(command=command):
                self.assert_stable(command, "UNSUPPORTED")

    def test_repeated_test_threads_is_unsupported_for_all_value_spellings(self):
        for command in self.REPEATED_TEST_THREADS:
            with self.subTest(command=command):
                self.assert_stable(command, "UNSUPPORTED")

    def test_option_arity_refusals_are_row_scoped_stable_and_actionable(self):
        commands = (
            self.CONFLICTING_IGNORE_MODES + self.REPEATED_EXACT
            + self.REPEATED_IGNORED + self.REPEATED_INCLUDE_IGNORED
            + self.REPEATED_TEST_THREADS
        )
        for index, command in enumerate(commands, 1):
            entry_id = f"s9-option-{index:02d}"
            with self.subTest(entry=entry_id, command=command):
                self.assert_stable(command, "UNSUPPORTED", entry_id=entry_id)

    def test_single_use_options_once_keep_resolving(self):
        for command in (
            "-p demo --lib tests::alpha_test -- --exact",
            "-p demo --lib -- --ignored quarantined_inline",
            "-p demo --lib -- --ignored outer::quarantined_outer_test",
            "-p demo --lib -- --include-ignored quarantined_inline",
            "-p demo --lib -- --include-ignored alpha_test",
            "-p demo --lib -- --include-ignored quarantined_inline alpha_test",
            "-p demo --lib alpha_test -- --test-threads 2",
            "-p demo --test smoke smoke_case -- --test-threads=1",
            "-p demo --lib -- --exact --ignored --test-threads=2 tests::quarantined_inline",
            "-p demo --lib -- --exact --include-ignored tests::alpha_test --test-threads 1",
        ):
            with self.subTest(command=command):
                self.assert_accepts(command)

    def test_repeated_skip_patterns_remain_supported(self):
        # --skip is repeatable: skip alpha/beta but leave gamma executable.
        # Include identical patterns and every separated/equals ordering.
        for command in (
            "-p demo --lib -- --skip alpha_test --skip beta_test",
            "-p demo --lib -- --skip=alpha_test --skip=beta_test",
            "-p demo --lib -- --skip alpha_test --skip=beta_test",
            "-p demo --lib -- --skip=alpha_test --skip beta_test",
            "-p demo --lib -- --skip=alpha_test --skip alpha_test",
            "-p demo --lib -- --exact --skip tests::alpha_test --skip=tests::beta_test tests::gamma_other",
        ):
            with self.subTest(command=command):
                self.assert_accepts(command)

    def test_repeated_skips_still_remove_all_matching_tests(self):
        # Accepting repeatable --skip must still apply every pattern, not
        # silently keep only the first or last and claim nonempty coverage.
        for command in (
            "-p demo --lib -- alpha_test beta_test --skip alpha_test --skip beta_test",
            "-p demo --lib -- alpha_test beta_test --skip=alpha_test --skip=beta_test",
            "-p demo --lib -- alpha_test beta_test --skip alpha_test --skip=beta_test",
            "-p demo --lib -- alpha_test beta_test --skip=alpha_test --skip beta_test",
        ):
            with self.subTest(command=command):
                self.assert_stable(command, "EMPTY_SELECTION")


class IgnoredSelectionTests(NoexecAcceptanceBase):
    """Effective selection honors ignore mode; empty executable fails."""

    def test_ignored_flag_selects_only_ignored(self):
        # --ignored intersects matched with ignored: inline, external and
        # integration ignored tests all resolve.
        self.assert_accepts("-p demo --lib -- --ignored quarantined_inline")
        self.assert_accepts("-p demo --lib -- --ignored quarantined_outer_test")
        self.assert_accepts("-p demo --lib -- --ignored quarantined")
        self.assert_accepts("-p demo --lib -- --ignored outer")
        self.assert_accepts("-p demo --test smoke -- --ignored")
        self.assert_accepts("-p demo --test smoke quarantined_smoke -- --ignored")

    def test_ignored_flag_with_only_nonignored_matches_fails(self):
        # --ignored with filters matching only nonignored tests executes
        # nothing (pinned libtest runs zero tests); fail with EMPTY_SELECTION.
        for command in (
            "-p demo --lib -- --ignored alpha",
            "-p demo --lib -- --ignored beta_test",
            "-p demo --lib -- --ignored alpha_test beta_test",
            "-p demo --test smoke smoke_case -- --ignored --test-threads=1",
            "-p demo --test smoke -- --ignored smoke_case",
        ):
            with self.subTest(command=command):
                self.assert_stable(command, "EMPTY_SELECTION")

    def test_default_excludes_ignored(self):
        # Without --ignored/--include-ignored, matched ignored tests are
        # reported ignored, not passed; ignored-only matches leave no
        # executable test behind.
        for command in (
            "-p demo --lib quarantined_inline",
            "-p demo --lib -- quarantined_outer_test",
            "-p demo --lib -- quarantined",
            "-p demo --test smoke quarantined_smoke",
            "-p demo --test smoke -- quarantined_smoke",
        ):
            with self.subTest(command=command):
                self.assert_stable(command, "EMPTY_SELECTION")
        # Mixed matches still resolve by excluding ignored.
        self.assert_accepts("-p demo --lib -- quarantined_inline alpha_test")
        self.assert_accepts("-p demo --lib -- quarantined alpha_test")

    def test_include_ignored_selects_all_matched(self):
        # --include-ignored runs both ignored and nonignored matches.
        self.assert_accepts("-p demo --lib -- --include-ignored quarantined")
        self.assert_accepts("-p demo --lib -- --include-ignored alpha")
        self.assert_accepts(
            "-p demo --lib -- --include-ignored quarantined_inline alpha_test")
        self.assert_accepts("-p demo --test smoke -- --include-ignored")
        self.assert_accepts(
            "-p demo --test smoke quarantined_smoke -- --include-ignored")
        self.assert_accepts(
            "-p demo --test smoke smoke_case -- --include-ignored")

    def test_exact_interactions_with_ignore_modes(self):
        # --exact applies to both positive filters and ignore-mode selection.
        self.assert_accepts(
            "-p demo --lib -- --exact --ignored tests::quarantined_inline")
        self.assert_accepts(
            "-p demo --lib -- --exact --ignored outer::quarantined_outer_test")
        self.assert_accepts(
            "-p demo --lib -- --exact --include-ignored tests::quarantined_inline")
        self.assert_accepts("-p demo --lib tests::alpha_test -- --exact")
        for command in (
            "-p demo --lib -- --exact --ignored tests::alpha_test",
            "-p demo --lib tests::quarantined_inline -- --exact",
            "-p demo --lib -- --exact tests::quarantined_inline",
        ):
            with self.subTest(command=command):
                self.assert_stable(command, "EMPTY_SELECTION")

    def test_skip_interactions_with_ignore_modes(self):
        # --skip removes from the executable set after ignore-mode filtering.
        self.assert_accepts(
            "-p demo --lib -- --ignored quarantined --skip quarantined_inline")
        self.assert_accepts(
            "-p demo --lib -- quarantined alpha_test --skip quarantined")
        for command in (
            "-p demo --lib -- --ignored alpha_test quarantined_inline --skip quarantined_inline",
            "-p demo --lib -- alpha_test quarantined_inline --skip alpha_test",
            "-p demo --test smoke -- --ignored --skip quarantined",
        ):
            with self.subTest(command=command):
                self.assert_stable(command, "EMPTY_SELECTION")

    def test_empty_ignored_selection_is_row_scoped_with_next_action(self):
        message = self.assert_rejects(
            "-p demo --lib -- --ignored alpha", "EMPTY_SELECTION",
            entry_id="s9-10")
        self.assertTrue(message.startswith("s9-10: "))
        message = self.assert_rejects(
            "-p demo --lib quarantined_inline", "EMPTY_SELECTION",
            entry_id="s9-11")
        self.assertTrue(message.startswith("s9-11: "))


class ValidControlsTests(NoexecAcceptanceBase):
    """Supported shapes keep resolving without subprocesses or network."""

    def test_fixture_filters_resolve_by_target(self):
        # One filter before -- is valid Cargo; several filters belong after --.
        self.assert_accepts("-p demo --lib alpha_test")
        self.assert_accepts("-p demo --lib -- beta_test gamma_other")
        self.assert_accepts("-p demo --test smoke smoke_case -- --exact")
        # Whole smoke target with --ignored selects the ignored smoke test.
        self.assert_accepts("-p demo --test smoke -- --ignored")

    def test_existing_libtest_flags_keep_resolving(self):
        # --ignored and --include-ignored appear in live checklist rows; the
        # --ignored controls match real ignored tests in this fixture.
        self.assert_accepts("-p demo --lib -- --ignored quarantined")
        self.assert_accepts("-p demo --lib -- --include-ignored beta")
        self.assert_accepts(
            "-p demo --test smoke quarantined_smoke -- --ignored --test-threads=1")
        self.assert_accepts("-p demo --lib tests::alpha_test -- --exact")

    def test_every_automated_checklist_row_still_resolves(self):
        automated = [e for e in self.checklist["entries"] if e["status"] == "automated"]
        self.assertGreater(len(automated), 0)
        for entry in automated:
            with self.subTest(entry=entry["id"]):
                checked(entry["verification"], entry["id"])

    def test_checklist_and_validator_stay_the_single_source_of_truth(self):
        counts = validate(self.parity, self.checklist)
        self.assertEqual(set(counts), set(range(1, 9)) | {12, 13})
        self.assertEqual(
            (ROOT / "docs/soak-checklist.md").read_text(), render(self.checklist))

    def test_static_selection_stays_distinct_from_run_evidence(self):
        # The rendered method line never claims an execution verdict, and the
        # maintaining-coverage notes still say selection is read offline.
        self.assertIn("(not an execution verdict)", render(self.checklist))
        instructions = "\n".join(self.checklist.get("instructions", []))
        self.assertIn("never run", instructions)


class SupportedCommandDocsTests(NoexecAcceptanceBase):
    """Agents find the bounded vocabulary, examples and offline command."""

    def test_validator_documents_supported_commands_and_refusals(self):
        doc = checker.__doc__ or ""
        self.assertIn("upported", doc, "validator must document its supported vocabulary")
        self.assertIn("cargo_cache.py run -- test", doc)
        self.assertIn("--no-run", doc)
        self.assertIn("--list", doc)
        self.assertIn("--skip", doc)
        self.assertIn("test_soak", doc, "validator must name its offline regression command")

    def test_checklist_instructions_document_supported_selection(self):
        instructions = "\n".join(self.checklist.get("instructions", []))
        self.assertIn("--no-run", instructions)
        self.assertIn("--list", instructions)
        self.assertIn("--skip", instructions)
        self.assertIn("upported", instructions)
        lowered = instructions.lower()
        self.assertTrue(
            "effective" in lowered or "selection" in lowered,
            "instructions must describe effective selected tests")


if __name__ == "__main__":
    unittest.main()
