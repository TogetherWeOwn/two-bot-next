"""Protected acceptance tests: Git-quoted Rust paths in the coverage gate.

Contract under test (the parser in ``check-patch-coverage.py`` must make
these pass without changing the threshold or the check wiring):

* Ordinary (``+++ b/...``) and Git C-quoted (``+++ "b/..."``) diff paths
  decode to the same repository-relative names coverage data (LCOV ``SF:``)
  uses, including Unicode (octal-escaped UTF-8) and escaped tab / quote /
  backslash characters.
* File state resets at every file boundary. An unsupported or malformed
  ``+++`` header fails closed (no added-lines result, a readable error):
  it must never attribute lines to the preceding file, and never let an
  uncovered source change fall through to the "no coverable changed
  lines" success path.
* A quoted source file alone, and following an ordinary file, contributes
  its correct added line numbers through the whole gate: uncovered lines
  fail hard mode at the unchanged default threshold, covered lines pass.
* Existing behavior is preserved: source/test-file classification,
  deletion-only files, LCOV validation (a blind gate fails closed), and
  the git diff command limits (zero context, Rust pathspec, timeout).

Diff parsing is exercised two ways: mocked ``git diff`` output with the
exact C-quoted header shapes Git emits, and a throwaway real Git repo
whose Unicode filename makes Git itself quote the header. The gate
verdict is exercised end to end through ``main()`` with LCOV fixtures,
not only through a decoder helper.
"""

import io
import subprocess
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from unittest import mock

import importlib.util

SPEC = importlib.util.spec_from_file_location(
    "coverage_gate_quoted", Path(__file__).with_name("check-patch-coverage.py")
)
gate = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(gate)

ORDINARY = "crates/bot/src/ordinary.rs"

# Decoded path -> the inside of the C-quoted "b/..." header Git emits.
# The quoted forms are raw diff text: backslash escapes are literal
# characters in the diff, not Python escapes in the decoded name.
QUOTED_CASES = {
    # e-acute is UTF-8 0xC3 0xA9 -> octal \303\251
    "crates/bot/src/café.rs": r"crates/bot/src/caf\303\251.rs",
    # CJK: U+65E5 = E6 97 A5, U+672C = E6 9C AC
    "crates/core/src/日本.rs": r"crates/core/src/\346\227\245\346\234\254.rs",
    # Escaped control / punctuation characters inside the file name.
    "crates/bot/src/with\ttab.rs": r"crates/bot/src/with\ttab.rs",
    'crates/bot/src/with"quote.rs': r"crates/bot/src/with\"quote.rs",
    "crates/bot/src/with\\slash.rs": r"crates/bot/src/with\\slash.rs",
}

UNICODE_PATH = "crates/bot/src/café.rs"
UNICODE_QUOTED = QUOTED_CASES[UNICODE_PATH]


def quoted_file_diff(quoted_inner, start, count):
    """One file's zero-context diff with a Git C-quoted +++ header."""
    added = "".join(f"+added line {start + i}\n" for i in range(count))
    return (
        f'diff --git "a/{quoted_inner}" "b/{quoted_inner}"\n'
        f'--- "a/{quoted_inner}"\n'
        f'+++ "b/{quoted_inner}"\n'
        f"@@ -0,0 +{start},{count} @@\n"
        f"{added}"
    )


def ordinary_file_diff(path, start, count):
    added = "".join(f"+added line {start + i}\n" for i in range(count))
    return (
        f"diff --git a/{path} b/{path}\n"
        f"--- a/{path}\n"
        f"+++ b/{path}\n"
        f"@@ -0,0 +{start},{count} @@\n"
        f"{added}"
    )


class _FakeProc:
    def __init__(self, stdout):
        self.returncode = 0
        self.stdout = stdout
        self.stderr = ""


def run_added_lines(diff_text):
    """gate.added_lines with git replaced by fixed diff output."""
    with mock.patch.object(
        gate.subprocess, "run", return_value=_FakeProc(diff_text)
    ) as run:
        added, error = gate.added_lines("base-ref", "head-ref")
    return added, error, run


def write_lcov(text):
    handle = tempfile.NamedTemporaryFile("w", suffix=".info", delete=False)
    handle.write(text)
    handle.close()
    return handle.name


def run_gate_main(diff_text, lcov_text, extra_args=(), root=None):
    """Full gate: real LCOV parsing + mocked git diff, through main()."""
    lcov = write_lcov(lcov_text)
    real_root = gate.ROOT
    if root is not None:
        gate.ROOT = root
    out = io.StringIO()
    try:
        with mock.patch.object(
            gate.subprocess, "run", return_value=_FakeProc(diff_text)
        ):
            with redirect_stdout(out):
                code = gate.main(["--lcov", lcov, *extra_args])
    finally:
        gate.ROOT = real_root
        Path(lcov).unlink()
    return code, out.getvalue()


def lcov_for(*records):
    """records: (path, {line: hit_count}) pairs -> LCOV text."""
    chunks = []
    for path, hits in records:
        chunks.append(f"TN:\nSF:{path}\n")
        for line in sorted(hits):
            chunks.append(f"DA:{line},{hits[line]}\n")
        chunks.append("end_of_record\n")
    return "".join(chunks)


class QuotedAddedLinesTests(unittest.TestCase):
    def test_quoted_paths_decode_to_coverage_names(self):
        for decoded, quoted_inner in QUOTED_CASES.items():
            with self.subTest(decoded=decoded):
                added, error, _ = run_added_lines(
                    quoted_file_diff(quoted_inner, 5, 2)
                )
                self.assertEqual(error, "")
                self.assertEqual(added, {(decoded, 5), (decoded, 6)})

    def test_quoted_file_alone_contributes_its_added_lines(self):
        added, error, _ = run_added_lines(
            quoted_file_diff(UNICODE_QUOTED, 1, 3)
        )
        self.assertEqual(error, "")
        self.assertEqual(
            added, {(UNICODE_PATH, 1), (UNICODE_PATH, 2), (UNICODE_PATH, 3)}
        )

    def test_quoted_file_after_ordinary_is_not_attributed_to_ordinary(self):
        diff = ordinary_file_diff(ORDINARY, 1, 1) + quoted_file_diff(
            UNICODE_QUOTED, 5, 2
        )
        added, error, _ = run_added_lines(diff)
        self.assertEqual(error, "")
        self.assertEqual(
            added, {(ORDINARY, 1), (UNICODE_PATH, 5), (UNICODE_PATH, 6)}
        )

    def test_ordinary_path_control_still_works(self):
        added, error, _ = run_added_lines(ordinary_file_diff(ORDINARY, 2, 2))
        self.assertEqual(error, "")
        self.assertEqual(added, {(ORDINARY, 2), (ORDINARY, 3)})

    def test_deletion_only_quoted_file_adds_nothing_and_next_file_is_clean(self):
        deleted = (
            f'diff --git "a/{UNICODE_QUOTED}" "b/{UNICODE_QUOTED}"\n'
            "deleted file mode 100644\n"
            f'--- "a/{UNICODE_QUOTED}"\n'
            "+++ /dev/null\n"
            "@@ -1,2 +0,0 @@\n"
            "-gone one\n"
            "-gone two\n"
        )
        diff = deleted + ordinary_file_diff(ORDINARY, 7, 1)
        added, error, _ = run_added_lines(diff)
        self.assertEqual(error, "")
        self.assertEqual(added, {(ORDINARY, 7)})

    def test_malformed_plus_header_fails_closed(self):
        for header in (
            "+++ ???not-a-path",
            '+++ "b/crates/bot/src/unterminated.rs',
            '+++ "b/crates/bot/src/bad\\xescape.rs"',
            "+++ ",
        ):
            with self.subTest(header=header):
                diff = (
                    "diff --git a/x.rs b/x.rs\n"
                    "--- a/x.rs\n"
                    f"{header}\n"
                    "@@ -0,0 +1 @@\n"
                    "+pub fn hidden() {}\n"
                )
                added, error, _ = run_added_lines(diff)
                self.assertIsNone(added)
                self.assertTrue(error)

    def test_malformed_header_after_ordinary_never_uses_previous_file(self):
        diff = ordinary_file_diff(ORDINARY, 1, 1) + (
            "diff --git a/y.rs b/y.rs\n"
            "--- a/y.rs\n"
            "+++ ???not-a-path\n"
            "@@ -0,0 +1 @@\n"
            "+pub fn hidden() {}\n"
        )
        added, error, _ = run_added_lines(diff)
        # Fail closed: neither silently dropped nor charged to ORDINARY.
        self.assertIsNone(added)
        self.assertTrue(error)

    def test_git_diff_command_keeps_its_limits(self):
        _, _, run = run_added_lines(ordinary_file_diff(ORDINARY, 1, 1))
        args, kwargs = run.call_args
        command = args[0]
        self.assertEqual(kwargs.get("timeout"), 120)
        self.assertIn("-U0", command)
        self.assertIn("*.rs", command)
        self.assertIn("base-ref", command)
        self.assertIn("head-ref", command)


class QuotedGateEndToEndTests(unittest.TestCase):
    def test_quoted_uncovered_lines_fail_hard(self):
        diff = quoted_file_diff(UNICODE_QUOTED, 5, 2)
        lcov = lcov_for((UNICODE_PATH, {5: 0, 6: 0}))
        code, output = run_gate_main(diff, lcov)
        self.assertEqual(code, 1)
        self.assertIn("::error::", output)
        self.assertIn(f"{UNICODE_PATH}:5", output)
        self.assertNotIn("no coverable changed lines", output)

    def test_quoted_covered_lines_pass(self):
        diff = quoted_file_diff(UNICODE_QUOTED, 5, 2)
        lcov = lcov_for((UNICODE_PATH, {5: 3, 6: 1}))
        code, output = run_gate_main(diff, lcov)
        self.assertEqual(code, 0)
        # Counted, not vacuous: both quoted lines must reach the ratio.
        self.assertIn("2/2 changed lines covered (100.0%)", output)
        self.assertNotIn("::error::", output)

    def test_covered_ordinary_does_not_mask_uncovered_quoted(self):
        diff = ordinary_file_diff(ORDINARY, 1, 1) + quoted_file_diff(
            UNICODE_QUOTED, 5, 2
        )
        lcov = lcov_for((ORDINARY, {1: 1}), (UNICODE_PATH, {5: 0, 6: 0}))
        code, output = run_gate_main(diff, lcov)
        self.assertEqual(code, 1)
        self.assertIn(f"{UNICODE_PATH}:5", output)
        self.assertNotIn(f"{ORDINARY}:5", output)

    def test_default_threshold_unchanged_for_quoted_paths(self):
        # 4 of 5 covered is exactly 80%: passes at the default threshold.
        diff = quoted_file_diff(UNICODE_QUOTED, 1, 5)
        lcov = lcov_for((UNICODE_PATH, {1: 1, 2: 1, 3: 1, 4: 1, 5: 0}))
        code, output = run_gate_main(diff, lcov)
        self.assertEqual(code, 0, output)
        self.assertIn("4/5 changed lines covered (80.0%)", output)
        # 3 of 4 covered is 75%: below the unchanged default, fails hard.
        diff = quoted_file_diff(UNICODE_QUOTED, 1, 4)
        lcov = lcov_for((UNICODE_PATH, {1: 1, 2: 1, 3: 1, 4: 0}))
        code, output = run_gate_main(diff, lcov)
        self.assertEqual(code, 1, output)
        self.assertIn("::error::", output)

    def test_quoted_measurable_source_without_lcov_record_is_uncovered(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source = root / UNICODE_PATH
            source.parent.mkdir(parents=True)
            source.write_text("pub fn café() { }\n")
            diff = quoted_file_diff(UNICODE_QUOTED, 1, 1)
            lcov = lcov_for((ORDINARY, {1: 1}))
            code, output = run_gate_main(diff, lcov, root=root)
        self.assertEqual(code, 1)
        self.assertIn("::error::", output)
        self.assertIn(f"{UNICODE_PATH}:1", output)
        self.assertNotIn("no coverable changed lines", output)

    def test_quoted_test_file_without_lcov_record_stays_unmeasured(self):
        test_path = "crates/bot/src/café_tests.rs"
        diff = quoted_file_diff(r"crates/bot/src/caf\303\251_tests.rs", 1, 1)
        lcov = lcov_for((ORDINARY, {1: 1}))
        code, output = run_gate_main(diff, lcov)
        self.assertEqual(code, 0, output)
        self.assertNotIn("::error::", output)
        self.assertNotIn(test_path, output)

    def test_malformed_lcov_still_fails_closed_with_quoted_diff(self):
        diff = quoted_file_diff(UNICODE_QUOTED, 5, 1)
        code, output = run_gate_main(diff, "garbage, no DA records\n")
        self.assertEqual(code, 1)
        self.assertIn("::error::", output)
        self.assertIn("blind", output)

    def test_malformed_diff_header_gate_fails_closed(self):
        diff = (
            "diff --git a/x.rs b/x.rs\n"
            "--- a/x.rs\n"
            "+++ ???not-a-path\n"
            "@@ -0,0 +1 @@\n"
            "+pub fn hidden() {}\n"
        )
        lcov = lcov_for((ORDINARY, {1: 1}))
        code, output = run_gate_main(diff, lcov)
        self.assertEqual(code, 1)
        self.assertIn("::error::", output)
        self.assertNotIn("no coverable changed lines", output)


class RealGitQuotedDiffTests(unittest.TestCase):
    def test_real_git_diff_with_unicode_filename(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for argv in (
                ["git", "init", "-q"],
                ["git", "config", "user.email", "t@t"],
                ["git", "config", "user.name", "t"],
                ["git", "config", "core.quotepath", "true"],
            ):
                subprocess.run(argv, cwd=root, check=True)
            source = root / UNICODE_PATH
            source.parent.mkdir(parents=True)
            source.write_text("pub fn a() {}\n", encoding="utf-8")
            (root / ORDINARY).write_text("pub fn o() {}\n")
            subprocess.run(["git", "add", "-A"], cwd=root, check=True)
            subprocess.run(["git", "commit", "-qm", "base"], cwd=root, check=True)
            base = subprocess.run(
                ["git", "rev-parse", "HEAD"], cwd=root,
                capture_output=True, text=True, check=True,
            ).stdout.strip()
            # Sanity: Git really does C-quote this header under quotepath,
            # so the fixture exercises the quoted shape, not plain UTF-8.
            source.write_text("pub fn a() {}\npub fn b() {}\n", encoding="utf-8")
            (root / ORDINARY).write_text("pub fn o() {}\npub fn p() {}\n")
            subprocess.run(["git", "add", "-A"], cwd=root, check=True)
            subprocess.run(["git", "commit", "-qm", "head"], cwd=root, check=True)
            head = subprocess.run(
                ["git", "rev-parse", "HEAD"], cwd=root,
                capture_output=True, text=True, check=True,
            ).stdout.strip()
            raw = subprocess.run(
                ["git", "-C", str(root), "diff", "-U0", "--no-color",
                 "--no-renames", base, head, "--", "*.rs"],
                capture_output=True, text=True, check=True,
            ).stdout
            self.assertIn(r'"b/crates/bot/src/caf\303\251.rs"', raw)
            real_root = gate.ROOT
            gate.ROOT = root
            try:
                added, error = gate.added_lines(base, head)
            finally:
                gate.ROOT = real_root
        self.assertEqual(error, "")
        self.assertEqual(added, {(UNICODE_PATH, 2), (ORDINARY, 2)})


if __name__ == "__main__":
    unittest.main()
