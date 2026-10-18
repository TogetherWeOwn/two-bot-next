"""Protected acceptance tests: floor-guard rename, quote and space evasions.

Contract under test (``collect`` and the assertion matcher in
``check-floor-guard.py`` must make these pass without weakening the
existing rules):

* Renaming a test file to a non-test path is a deleted test. When the
  rename also guts assertions, the stripped assertions count too: the
  verdict carries both ``deleted-test`` and ``assertion-removed``.
  A pure rename from one test path to another stays clean.
* Paths Git C-quotes (``+++ "b/..."`` with octal-escaped UTF-8) and
  paths with spaces (``+++`` header followed by a tab) decode to the
  exact repository-relative names, in added, removed and deleted
  results alike.
* Untracked files are collected under their real names even when the
  name contains a space or non-ASCII characters, so a waiver hiding
  in one is still a ``silenced-checker``.
* ``debug_assert!`` / ``debug_assert_eq!`` / ``debug_assert_ne!`` are
  assertions: removing one from test code is ``assertion-removed``,
  moving one to another test file is not, and removing one outside
  test code stays out of scope.

Positive controls for the behavior that already works are in the same
file and must stay green throughout: plain assertion stripping through
``scan`` and through a real Git history, a spaced deleted test file, a
waiver in an ordinarily named untracked file, and a clean commit.

Fixtures are miniature real Git repositories plus synthetic line sets,
mirroring ``make_repo`` in ``test_check_floor_guard.py``. Offline only.

Fixture strings keep waiver text inside expressions, never at the start
of a physical line, so this file's own diff does not trip the guard it
tests.
"""

import importlib.util
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

SPEC = importlib.util.spec_from_file_location(
    "floor_guard_evasions", Path(__file__).with_name("check-floor-guard.py")
)
gate = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(gate)

TEST_FILE = "crates/core/tests/store.rs"
RENAMED_TEST_FILE = "crates/core/tests/store_v2.rs"
NON_TEST_FILE = "crates/core/src/store_helper.rs"
SPACED_TEST_FILE = "crates/core/tests/spaced name.rs"
UNICODE_TEST_FILE = "crates/bot/src/café_tests.rs"

FILLER = "".join(f"// filler line {i}\n" for i in range(20))
TEST_BODY = (
    FILLER
    + "#[test]\n"
    + "fn it() {\n"
    + "    assert_eq!(got, want);\n"
    + "    assert!(ready);\n"
    + "}\n"
)
GUTTED_BODY = FILLER + "fn it() {\n}\n"
WAIVER_BODY = "#[" + "allow(dead_code)]\nfn stale() {}\n"


def git(*args, cwd, env=None):
    subprocess.run(
        ["git", *args],
        cwd=cwd,
        check=True,
        capture_output=True,
        text=True,
        env=env,
    )


def head_sha(root):
    return subprocess.check_output(
        ["git", "rev-parse", "HEAD"], cwd=root, text=True
    ).strip()


def make_repo():
    tmp = tempfile.TemporaryDirectory()
    root = Path(tmp.name)
    environ = {
        "GIT_AUTHOR_NAME": "t",
        "GIT_AUTHOR_EMAIL": "t@t",
        "GIT_COMMITTER_NAME": "t",
        "GIT_COMMITTER_EMAIL": "t@t",
    }
    git("init", "-b", "main", cwd=root)
    git("config", "commit.gpgsign", "false", cwd=root)
    (root / "src").mkdir()
    (root / "src" / "main.rs").write_text("fn main() {}\n")
    git("add", ".", cwd=root)
    git("commit", "-m", "base", cwd=root, env={**os.environ, **environ})
    return tmp, root, {**os.environ, **environ}


def write(root, path, text):
    target = root / path
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(text, encoding="utf-8")


def commit_all(root, env, message):
    git("add", ".", cwd=root)
    git("commit", "-m", message, cwd=root, env=env)
    return head_sha(root)


class DebugAssertScanTests(unittest.TestCase):
    def test_removed_plain_assertion_still_fails(self):
        # Positive control: existing behavior.
        errors = gate.scan(
            [],
            [(TEST_FILE, "    assert_eq!(got, want);")],
            [],
        )
        self.assertTrue(any("assertion-removed" in e for e in errors), errors)

    def test_deleted_test_file_still_fails(self):
        # Positive control: existing behavior.
        errors = gate.scan([], [], [TEST_FILE])
        self.assertTrue(any("deleted-test" in e for e in errors), errors)

    def test_removed_debug_assertions_fail(self):
        for line in (
            "    debug_assert!(ready);",
            "    debug_assert_eq!(got, want);",
            "    debug_assert_ne!(got, bad);",
        ):
            with self.subTest(line=line):
                errors = gate.scan([], [(TEST_FILE, line)], [])
                self.assertTrue(
                    any("assertion-removed" in e for e in errors), errors
                )

    def test_moved_debug_assertion_passes(self):
        errors = gate.scan(
            [("crates/core/tests/other.rs", "    debug_assert_eq!(got, want);")],
            [(TEST_FILE, "    debug_assert_eq!(got, want);")],
            [],
        )
        self.assertEqual(errors, [])

    def test_removed_debug_assertion_outside_tests_passes(self):
        errors = gate.scan(
            [],
            [("src/handler.rs", "    debug_assert_eq!(got, want);")],
            [],
        )
        self.assertEqual(errors, [])


class RenameEvasionTests(unittest.TestCase):
    def test_rename_to_non_test_path_while_gutting_fails_twice(self):
        tmp, root, env = make_repo()
        try:
            write(root, TEST_FILE, TEST_BODY)
            base = commit_all(root, env, "test file")

            (root / TEST_FILE).unlink()
            write(root, NON_TEST_FILE, GUTTED_BODY)
            commit_all(root, env, "rename and gut")

            errors = gate.check(root, base)
        finally:
            tmp.cleanup()
        self.assertTrue(any("deleted-test" in e for e in errors), errors)
        self.assertTrue(any("assertion-removed" in e for e in errors), errors)

    def test_rename_to_non_test_path_intact_is_a_deleted_test(self):
        tmp, root, env = make_repo()
        try:
            write(root, TEST_FILE, TEST_BODY)
            base = commit_all(root, env, "test file")

            (root / TEST_FILE).unlink()
            write(root, NON_TEST_FILE, TEST_BODY)
            commit_all(root, env, "rename intact")

            errors = gate.check(root, base)
        finally:
            tmp.cleanup()
        self.assertTrue(any("deleted-test" in e for e in errors), errors)

    def test_pure_rename_between_test_paths_passes(self):
        # Positive control: unchanged behavior must survive the fix.
        tmp, root, env = make_repo()
        try:
            write(root, TEST_FILE, TEST_BODY)
            base = commit_all(root, env, "test file")

            (root / TEST_FILE).unlink()
            write(root, RENAMED_TEST_FILE, TEST_BODY)
            commit_all(root, env, "rename within tests")

            errors = gate.check(root, base)
        finally:
            tmp.cleanup()
        self.assertEqual(errors, [])


class QuotedAndSpacedPathTests(unittest.TestCase):
    def test_collect_decodes_quoted_and_spaced_paths(self):
        tmp, root, env = make_repo()
        try:
            write(
                root,
                SPACED_TEST_FILE,
                "#[test]\nfn s() {\n    assert_eq!(a, b);\n}\n",
            )
            write(
                root,
                UNICODE_TEST_FILE,
                "#[test]\nfn c() {\n    assert_eq!(a, b);\n}\n",
            )
            base = commit_all(root, env, "spaced and unicode tests")

            write(
                root,
                SPACED_TEST_FILE,
                "#[test]\nfn s() {\n    assert_eq!(a, expected);\n}\n",
            )
            write(
                root,
                UNICODE_TEST_FILE,
                "#[test]\nfn c() {\n    assert_eq!(a, expected);\n}\n",
            )
            commit_all(root, env, "edit assertions")

            added, removed, _deleted = gate.collect(root, base, "HEAD")
        finally:
            tmp.cleanup()
        for path in (SPACED_TEST_FILE, UNICODE_TEST_FILE):
            with self.subTest(path=path):
                self.assertIn((path, "    assert_eq!(a, expected);"), added)
                self.assertIn((path, "    assert_eq!(a, b);"), removed)

    def test_deleted_quoted_test_file_names_the_decoded_path(self):
        tmp, root, env = make_repo()
        try:
            write(
                root,
                UNICODE_TEST_FILE,
                "#[test]\nfn c() {\n    assert_eq!(a, b);\n}\n",
            )
            base = commit_all(root, env, "unicode test")

            (root / UNICODE_TEST_FILE).unlink()
            commit_all(root, env, "delete unicode test")

            errors = gate.check(root, base)
        finally:
            tmp.cleanup()
        deleted = [e for e in errors if "deleted-test" in e]
        self.assertTrue(deleted, errors)
        self.assertTrue(
            any(UNICODE_TEST_FILE in e for e in deleted), errors
        )

    def test_deleted_spaced_test_file_fails(self):
        # Positive control: name-status already carries spaced names.
        tmp, root, env = make_repo()
        try:
            write(
                root,
                SPACED_TEST_FILE,
                "#[test]\nfn s() {\n    assert_eq!(a, b);\n}\n",
            )
            base = commit_all(root, env, "spaced test")

            (root / SPACED_TEST_FILE).unlink()
            commit_all(root, env, "delete spaced test")

            errors = gate.check(root, base)
        finally:
            tmp.cleanup()
        deleted = [e for e in errors if "deleted-test" in e]
        self.assertTrue(deleted, errors)
        self.assertTrue(any(SPACED_TEST_FILE in e for e in deleted), errors)


class UntrackedFileTests(unittest.TestCase):
    def check_untracked(self, name):
        tmp, root, _env = make_repo()
        try:
            write(root, name, WAIVER_BODY)
            return gate.check(root, "HEAD")
        finally:
            tmp.cleanup()

    def test_untracked_waiver_in_ordinary_name_fails(self):
        # Positive control: existing behavior.
        errors = self.check_untracked("src/helper.rs")
        self.assertTrue(any("silenced-checker" in e for e in errors), errors)

    def test_untracked_waiver_in_spaced_name_fails(self):
        errors = self.check_untracked("src/spaced name.rs")
        waived = [e for e in errors if "silenced-checker" in e]
        self.assertTrue(waived, errors)
        self.assertTrue(any("src/spaced name.rs" in e for e in waived), errors)

    def test_untracked_waiver_in_unicode_name_fails(self):
        errors = self.check_untracked("src/café.rs")
        waived = [e for e in errors if "silenced-checker" in e]
        self.assertTrue(waived, errors)
        self.assertTrue(any("src/café.rs" in e for e in waived), errors)


class OrdinaryGitControlTests(unittest.TestCase):
    def test_clean_commit_passes(self):
        tmp, root, env = make_repo()
        try:
            write(root, "src/main.rs", "fn main() {\n    println!(\"hi\");\n}\n")
            commit_all(root, env, "clean")
            errors = gate.check(root, "HEAD~1")
        finally:
            tmp.cleanup()
        self.assertEqual(errors, [])

    def test_assertion_stripped_through_git_fails(self):
        tmp, root, env = make_repo()
        try:
            write(root, TEST_FILE, TEST_BODY)
            base = commit_all(root, env, "test file")

            write(root, TEST_FILE, GUTTED_BODY)
            commit_all(root, env, "gut in place")

            errors = gate.check(root, base)
        finally:
            tmp.cleanup()
        self.assertTrue(any("assertion-removed" in e for e in errors), errors)


if __name__ == "__main__":
    unittest.main()
