"""Supplemental acceptance tests: spaced Rust paths carry a trailing TAB.

Git appends a single TAB to the ``---``/``+++`` header lines whenever the
path contains a space (verified with Git 2.47.3, ``-U0 --no-renames``,
``core.quotepath=true``), for both plain and C-quoted headers. The tab is
a header terminator, not part of the file name. These tests prove the
parser strips it before decoding, so spaced names map to their LCOV
records and uncovered lines fail hard instead of falling through to the
"no coverable changed lines" success path.

Owned by TOG-28205. The protected quoted-path suite
(``test_check_patch_coverage_quoted_paths_acceptance.py``) is unchanged.
"""

import importlib.util
import io
import subprocess
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from unittest import mock

SPEC = importlib.util.spec_from_file_location(
    "coverage_gate_spaced", Path(__file__).with_name("check-patch-coverage.py")
)
gate = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(gate)

ASCII_SPACED = "crates/bot/src/x y.rs"
UNICODE_SPACED = "crates/bot/src/café z.rs"
# C-quoted inner form Git emits for the Unicode spaced name
# (e-acute is UTF-8 0xC3 0xA9 -> octal \303\251).
UNICODE_SPACED_QUOTED = r"crates/bot/src/caf\303\251 z.rs"


class _FakeProc:
    def __init__(self, stdout):
        self.returncode = 0
        self.stdout = stdout
        self.stderr = ""


def run_added_lines(diff_text):
    with mock.patch.object(
        gate.subprocess, "run", return_value=_FakeProc(diff_text)
    ):
        return gate.added_lines("base-ref", "head-ref")


def write_lcov(text):
    handle = tempfile.NamedTemporaryFile("w", suffix=".info", delete=False)
    handle.write(text)
    handle.close()
    return handle.name


def run_gate_main(diff_text, lcov_text, root=None):
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
                code = gate.main(["--lcov", lcov])
    finally:
        gate.ROOT = real_root
        Path(lcov).unlink()
    return code, out.getvalue()


def lcov_for(*records):
    chunks = []
    for path, hits in records:
        chunks.append(f"TN:\nSF:{path}\n")
        for line in sorted(hits):
            chunks.append(f"DA:{line},{hits[line]}\n")
        chunks.append("end_of_record\n")
    return "".join(chunks)


def spaced_diff_text():
    """Exact shapes Git emits for spaced names, trailing TAB included."""
    return (
        "diff --git a/crates/bot/src/x y.rs b/crates/bot/src/x y.rs\n"
        "--- a/crates/bot/src/x y.rs\t\n"
        "+++ b/crates/bot/src/x y.rs\t\n"
        "@@ -0,0 +5,2 @@\n"
        "+added one\n"
        "+added two\n"
        f'diff --git "a/{UNICODE_SPACED_QUOTED}" "b/{UNICODE_SPACED_QUOTED}"\n'
        f'--- "a/{UNICODE_SPACED_QUOTED}"\t\n'
        f'+++ "b/{UNICODE_SPACED_QUOTED}"\t\n'
        "@@ -0,0 +5,2 @@\n"
        "+added one\n"
        "+added two\n"
    )


class SpacedHeaderParsingTests(unittest.TestCase):
    def test_parse_strips_trailing_tab_from_plain_header(self):
        self.assertEqual(
            gate._parse_new_file_header("+++ b/crates/bot/src/x y.rs\t"),
            ASCII_SPACED,
        )

    def test_parse_strips_trailing_tab_from_quoted_header(self):
        self.assertEqual(
            gate._parse_new_file_header(f'+++ "b/{UNICODE_SPACED_QUOTED}"\t'),
            UNICODE_SPACED,
        )

    def test_added_lines_with_trailing_tabs_have_clean_names(self):
        added, error = run_added_lines(spaced_diff_text())
        self.assertEqual(error, "")
        self.assertEqual(
            added,
            {
                (ASCII_SPACED, 5),
                (ASCII_SPACED, 6),
                (UNICODE_SPACED, 5),
                (UNICODE_SPACED, 6),
            },
        )


class SpacedGateEndToEndTests(unittest.TestCase):
    def test_spaced_uncovered_lines_fail_hard_not_vacuous(self):
        diff = spaced_diff_text()
        lcov = lcov_for(
            (ASCII_SPACED, {5: 0, 6: 0}),
            (UNICODE_SPACED, {5: 0, 6: 0}),
        )
        code, output = run_gate_main(diff, lcov)
        self.assertEqual(code, 1)
        self.assertIn("::error::", output)
        self.assertIn(f"{ASCII_SPACED}:5", output)
        self.assertIn(f"{UNICODE_SPACED}:5", output)
        self.assertNotIn("no coverable changed lines", output)

    def test_spaced_covered_lines_pass_with_counts(self):
        diff = spaced_diff_text()
        lcov = lcov_for(
            (ASCII_SPACED, {5: 1, 6: 2}),
            (UNICODE_SPACED, {5: 1, 6: 2}),
        )
        code, output = run_gate_main(diff, lcov)
        self.assertEqual(code, 0, output)
        self.assertIn("4/4 changed lines covered (100.0%)", output)


class RealGitSpacedDiffTests(unittest.TestCase):
    def test_real_git_spaced_names_decode_without_tab(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for argv in (
                ["git", "init", "-q"],
                ["git", "config", "user.email", "t@t"],
                ["git", "config", "user.name", "t"],
                ["git", "config", "core.quotepath", "true"],
            ):
                subprocess.run(argv, cwd=root, check=True)
            ascii_src = root / ASCII_SPACED
            uni_src = root / UNICODE_SPACED
            ascii_src.parent.mkdir(parents=True, exist_ok=True)
            ascii_src.write_text("pub fn a() {}\n", encoding="utf-8")
            uni_src.write_text("pub fn a() {}\n", encoding="utf-8")
            subprocess.run(["git", "add", "-A"], cwd=root, check=True)
            subprocess.run(["git", "commit", "-qm", "base"], cwd=root, check=True)
            base = subprocess.run(
                ["git", "rev-parse", "HEAD"],
                cwd=root, capture_output=True, text=True, check=True,
            ).stdout.strip()
            ascii_src.write_text("pub fn a() {}\npub fn b() {}\n", encoding="utf-8")
            uni_src.write_text("pub fn a() {}\npub fn b() {}\n", encoding="utf-8")
            subprocess.run(["git", "add", "-A"], cwd=root, check=True)
            subprocess.run(["git", "commit", "-qm", "head"], cwd=root, check=True)
            head = subprocess.run(
                ["git", "rev-parse", "HEAD"],
                cwd=root, capture_output=True, text=True, check=True,
            ).stdout.strip()
            raw = subprocess.run(
                ["git", "-C", str(root), "diff", "-U0", "--no-color",
                 "--no-renames", base, head, "--", "*.rs"],
                capture_output=True, text=True, check=True,
            ).stdout
            # Sanity: Git really emits the trailing TAB for both shapes.
            self.assertIn("+++ b/crates/bot/src/x y.rs\t", raw)
            self.assertIn(f'+++ "b/{UNICODE_SPACED_QUOTED}"\t', raw)
            real_root = gate.ROOT
            gate.ROOT = root
            try:
                added, error = gate.added_lines(base, head)
            finally:
                gate.ROOT = real_root
        self.assertEqual(error, "")
        self.assertEqual(added, {(ASCII_SPACED, 2), (UNICODE_SPACED, 2)})


if __name__ == "__main__":
    unittest.main()
