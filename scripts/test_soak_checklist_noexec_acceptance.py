"""Protected acceptance tests: reject soak commands that execute no selected tests.

Acceptance-owned: preserve this file in the combined implementation change;
return suspected test defects to its author, rather than weakening assertions.
This file is intentionally RED until the implementation satisfies the contract.

Contract (offline, stdlib only, no Cargo, subprocesses or network):

* Refuse Cargo ``--no-run`` and libtest ``--list`` as automated soak coverage.
  The validator uses an explicit bounded supported-option vocabulary and
  enforces value arity, including after ``--``.
* Compute nonempty effective selected tests after positive filters, ``--exact``
  and ``--skip``. A selected test that is entirely skipped cannot satisfy
  coverage; a supported skip leaving real tests passes. Fail closed for
  unsupported semantics (misplaced known options).
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
  ``--exact`` and ``--skip``.
* ``UNKNOWN_OPTION``: a flag outside the bounded supported vocabulary.
* ``MISSING_VALUE``: a value option with no value token (``--skip`` or
  ``--test-threads`` at the end, ``--test`` or ``-p`` without a value).
* ``INVALID_VALUE``: a value option with an empty or malformed value
  (``--test-threads`` not a positive integer, ``--skip=`` empty, or a flag
  given ``=value`` such as ``--exact=foo``, ``--ignored=foo``, ``--lib=foo``).
* ``UNSUPPORTED``: a known option in the wrong position (libtest ``--skip`` /
  ``--exact`` / ``--ignored`` before ``--``; Cargo ``-p`` / ``--test`` /
  ``--features`` after ``--``).

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
        #[cfg(test)]
        mod tests {
            #[test]
            fn alpha_test() {}
            #[test]
            fn beta_test() {}
            #[test]
            fn gamma_other() {}
        }
    """,
    "crates/demo/tests/smoke.rs": """
        #[test] fn smoke_case() {}
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
        # "test" matches all three lib tests (via "tests::" and the fn names);
        # skipping "test" leaves nothing.
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
        # "test" matches alpha/beta/gamma; skipping only alpha leaves two.
        self.assert_accepts("-p demo --lib test -- --skip alpha_test")
        self.assert_accepts("-p demo --lib alpha_test beta_test -- --skip alpha")
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


class ValidControlsTests(NoexecAcceptanceBase):
    """Supported shapes keep resolving without subprocesses or network."""

    def test_fixture_filters_resolve_by_target(self):
        self.assert_accepts("-p demo --lib alpha_test")
        self.assert_accepts("-p demo --lib beta_test gamma_other")
        self.assert_accepts("-p demo --test smoke smoke_case -- --exact")
        self.assert_accepts("-p demo --test smoke -- --ignored")

    def test_existing_libtest_flags_keep_resolving(self):
        # --ignored and --include-ignored appear in live checklist rows.
        self.assert_accepts("-p demo --lib -- --ignored alpha")
        self.assert_accepts("-p demo --lib -- --include-ignored beta")
        self.assert_accepts("-p demo --test smoke smoke_case -- --ignored --test-threads=1")
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
