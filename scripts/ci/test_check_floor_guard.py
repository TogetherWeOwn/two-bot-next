"""Offline tests for the floor-guard HARD gate (CONSTRAINTS.md floor).

TDD: these fixtures were written before the gate itself. They feed synthetic
added/removed line sets plus miniature git repos through the gate and assert
it fails added suppressions, added skips, deleted test files, stripped
assertions and lowered CONSTRAINTS.md thresholds, while passing clean diffs,
tightened thresholds and markdown prose.

Only the inline-suppression fixtures are string-concatenated: any contiguous
suppression literal in this file's own committed diff would trip the guard it
tests (the fixture strings are data, not suppressions). Rust attributes need
no such treatment because the gate only honours them at line start.
"""

import importlib.util
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]

SPEC = importlib.util.spec_from_file_location(
    "floor_guard", Path(__file__).with_name("check-floor-guard.py")
)
gate = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(gate)

# Inline suppressions a real checker would honour mid-line. Concatenated so
# this file never contains the contiguous literal the guard scans for.
ESLINT_OFF = "eslint-" + "disable-next-line no-console"
NOQA = "# " + "noqa: F401"
GITLEAKS_ALLOW = "gitleaks:" + "allow"
NOSEMGREP = "nose" + "mgrep"


def added(path, *lines):
    return [(path, line) for line in lines]


class ScanTests(unittest.TestCase):
    def test_clean_diff_passes(self):
        errors = gate.scan(
            added("src/handler.rs", "fn handle() {", "    Ok(())", "}"),
            [],
            [],
        )
        self.assertEqual(errors, [])

    def test_rust_allow_attribute_fails(self):
        errors = gate.scan(
            added("src/handler.rs", "#[allow(dead_code)]", "fn stale() {}"),
            [],
            [],
        )
        self.assertEqual(len(errors), 1)
        self.assertIn("silenced-checker", errors[0])
        self.assertIn("src/handler.rs", errors[0])

    def test_crate_level_allow_attribute_fails(self):
        errors = gate.scan(
            added("src/lib.rs", "#![allow(clippy::all)]"),
            [],
            [],
        )
        self.assertTrue(any("silenced-checker" in e for e in errors), errors)

    def test_prose_mention_of_allow_is_not_a_suppression(self):
        errors = gate.scan(
            added("src/handler.rs", "// prefer expect() over allow attributes"),
            [],
            [],
        )
        self.assertEqual(errors, [])

    def test_eslint_disable_fails(self):
        errors = gate.scan(
            added("wrangler/ui.mjs", "// " + ESLINT_OFF),
            [],
            [],
        )
        self.assertTrue(any("silenced-checker" in e for e in errors), errors)

    def test_noqa_and_gitleaks_allow_fail(self):
        for marker in (NOQA, GITLEAKS_ALLOW, NOSEMGREP):
            with self.subTest(marker=marker):
                errors = gate.scan(
                    added("scripts/ci/helper.py", "token = x  " + marker),
                    [],
                    [],
                )
                self.assertTrue(
                    any("silenced-checker" in e for e in errors), errors
                )

    def test_added_ignore_attribute_fails(self):
        errors = gate.scan(
            added("crates/core/tests/store.rs", "#[ignore]", "#[test]", "fn it() {}"),
            [],
            [],
        )
        self.assertTrue(any("test-made-easier" in e for e in errors), errors)

    def test_added_skip_call_fails(self):
        # Concatenated like the module-level markers: the contiguous call
        # would trip the guard on this file's own committed diff.
        errors = gate.scan(
            added("wrangler/ui.test.mjs", "x" + "it('renders', () => {});"),
            [],
            [],
        )
        self.assertTrue(any("test-made-easier" in e for e in errors), errors)

    def test_exit_call_is_not_a_skip(self):
        errors = gate.scan(
            added("scripts/ci/check-x.py", "    raise SystemExit(main())"),
            [],
            [],
        )
        self.assertEqual(errors, [])

    def test_semgrep_exit_pattern_is_not_a_skip(self):
        errors = gate.scan(
            added(".semgrep/rust-warn.yml", "      - pattern: process::exit(...)"),
            [],
            [],
        )
        self.assertEqual(errors, [])

    def test_deleted_test_file_fails(self):
        errors = gate.scan([], [], ["crates/core/tests/store.rs"])
        self.assertEqual(len(errors), 1)
        self.assertIn("deleted-test", errors[0])

    def test_deleted_non_test_file_passes(self):
        self.assertEqual(gate.scan([], [], ["docs/old-note.md"]), [])

    def test_removed_assertion_fails(self):
        errors = gate.scan(
            [],
            [("crates/core/tests/store.rs", "    assert_eq!(got, want);")],
            [],
        )
        self.assertEqual(len(errors), 1)
        self.assertIn("assertion-removed", errors[0])

    def test_removed_ordinary_line_passes(self):
        errors = gate.scan(
            [],
            [("crates/core/tests/store.rs", "    let got = load();")],
            [],
        )
        self.assertEqual(errors, [])

    def test_removed_assertion_outside_tests_passes(self):
        errors = gate.scan(
            [],
            [("src/handler.rs", "    assert_eq!(got, want);")],
            [],
        )
        self.assertEqual(errors, [])

    def test_lowered_threshold_fails(self):
        errors = gate.scan(
            added("CONSTRAINTS.md", "| Coverage | Changed lines >= 70% covered |"),
            [("CONSTRAINTS.md", "| Coverage | Changed lines >= 80% covered |")],
            [],
        )
        self.assertEqual(len(errors), 1)
        self.assertIn("threshold-lowered", errors[0])

    def test_raised_threshold_passes(self):
        errors = gate.scan(
            added("CONSTRAINTS.md", "| Coverage | Changed lines >= 90% covered |"),
            [("CONSTRAINTS.md", "| Coverage | Changed lines >= 80% covered |")],
            [],
        )
        self.assertEqual(errors, [])

    def test_numbers_outside_constraints_are_not_thresholds(self):
        errors = gate.scan(
            added("docs/plan.md", "coverage fell from 80% to 70% this week"),
            [("docs/plan.md", "coverage is 80% this week")],
            [],
        )
        self.assertEqual(errors, [])

    def test_markdown_prose_may_mention_suppressions(self):
        errors = gate.scan(
            added(
                "docs/guide.md",
                "Never add #[allow(dead_code)] or #[ignore] to get green.",
            ),
            [],
            [],
        )
        self.assertEqual(errors, [])


def git(*args, cwd, env=None):
    subprocess.run(
        ["git", *args],
        cwd=cwd,
        check=True,
        capture_output=True,
        text=True,
        env=env,
    )


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
    git("commit", "-m", "base", cwd=root, env={**__import__("os").environ, **environ})
    return tmp, root, environ


class GitPlumbingTests(unittest.TestCase):
    def test_missing_base_raises_no_merge_base(self):
        tmp, root, _ = make_repo()
        try:
            with self.assertRaises(gate.NoMergeBase):
                gate.check(root, "origin/main")
        finally:
            tmp.cleanup()

    def test_main_exits_2_without_a_merge_base(self):
        tmp, root, _ = make_repo()
        try:
            with self.assertRaises(SystemExit) as ctx:
                gate.main(["--root", str(root), "--base-ref", "origin/main"])
            self.assertEqual(ctx.exception.code, 2)
        finally:
            tmp.cleanup()

    def test_check_flags_a_suppression_through_git(self):
        tmp, root, environ = make_repo()
        try:
            (root / "src" / "main.rs").write_text(
                "#[allow(dead_code)]\nfn stale() {}\nfn main() {}\n"
            )
            git("add", ".", cwd=root)
            git(
                "commit",
                "-m",
                "suppression",
                cwd=root,
                env={**__import__("os").environ, **environ},
            )
            errors = gate.check(root, "HEAD~1")
        finally:
            tmp.cleanup()
        self.assertEqual(len(errors), 1)
        self.assertIn("silenced-checker", errors[0])

    def test_check_passes_a_clean_commit(self):
        tmp, root, environ = make_repo()
        try:
            (root / "src" / "main.rs").write_text("fn main() {\n    println!(\"hi\");\n}\n")
            git("add", ".", cwd=root)
            git(
                "commit",
                "-m",
                "clean",
                cwd=root,
                env={**__import__("os").environ, **environ},
            )
            errors = gate.check(root, "HEAD~1")
        finally:
            tmp.cleanup()
        self.assertEqual(errors, [])

    def test_pull_request_head_excludes_unrelated_base_branch_changes(self):
        tmp, root, environ = make_repo()
        commit_env = {**__import__("os").environ, **environ}
        try:
            base = subprocess.check_output(
                ["git", "rev-parse", "HEAD"], cwd=root, text=True
            ).strip()
            git("checkout", "-b", "pr", cwd=root)
            (root / "src" / "pr.rs").write_text("fn pr() {}\n")
            git("add", ".", cwd=root)
            git("commit", "-m", "PR change", cwd=root, env=commit_env)
            pr_head = subprocess.check_output(
                ["git", "rev-parse", "HEAD"], cwd=root, text=True
            ).strip()

            git("checkout", "main", cwd=root)
            (root / "scripts").mkdir()
            waiver = "import example  # " + "noqa: E402\n"
            (root / "scripts" / "test_upstream.py").write_text(waiver)
            git("add", ".", cwd=root)
            git("commit", "-m", "unrelated base change", cwd=root, env=commit_env)
            git("merge", "--no-ff", "pr", "-m", "PR merge", cwd=root, env=commit_env)

            merge_errors = gate.check(root, base)
            pr_errors = gate.check(root, base, pr_head)
        finally:
            tmp.cleanup()
        self.assertTrue(any("silenced-checker" in error for error in merge_errors))
        self.assertEqual(pr_errors, [])


if __name__ == "__main__":
    unittest.main()
