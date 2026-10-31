"""Offline tests for the patch-coverage HARD gate.

The gate must fail below the threshold, stay silent at or above
it, pass when there is nothing coverable to measure, and fail closed
when it is blind. lcov parsing and the covered/coverable math run against
fixtures; diff parsing runs against a throwaway git repo.
"""

import io
import subprocess
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path

import importlib.util

SPEC = importlib.util.spec_from_file_location(
    "coverage_gate", Path(__file__).with_name("check-patch-coverage.py")
)
gate = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(gate)


LCOV = """TN:
SF:crates/bot/src/gateway_commands.rs
DA:1,1
DA:2,0
DA:3,5
end_of_record
TN:
SF:/abs/path/crates/core/src/router.rs
DA:10,0
end_of_record
"""


def write_lcov(text):
    handle = tempfile.NamedTemporaryFile("w", suffix=".info", delete=False)
    handle.write(text)
    handle.close()
    return handle.name


class LcovTests(unittest.TestCase):
    def test_hit_counts_keyed_by_file_and_line(self):
        lcov = write_lcov(LCOV)
        try:
            hits, error = gate.read_lcov(lcov)
        finally:
            Path(lcov).unlink()
        self.assertEqual(error, "")
        self.assertEqual(hits[("crates/bot/src/gateway_commands.rs", 1)], 1)
        self.assertEqual(hits[("crates/bot/src/gateway_commands.rs", 2)], 0)
        self.assertEqual(hits[("/abs/path/crates/core/src/router.rs", 10)], 0)

    def test_missing_report_is_blindness(self):
        hits, error = gate.read_lcov("/nonexistent/lcov.info")
        self.assertIsNone(hits)
        self.assertTrue(error)

    def test_garbage_lcov_without_data_records_is_blindness(self):
        for text in (
            "garbage",
            "SF:src/lib.rs\nend_of_record\n",
            "SF:src/lib.rs\nDA:not-a-line,1\n",
            "SF:src/lib.rs\nDA:1\n",
            "SF:src/lib.rs\nDA:0,0\n",
            "SF:src/lib.rs\nDA:-1,1\n",
            "SF:src/lib.rs\nDA:1,-1\n",
            "SF:\nDA:1,1\nend_of_record\n",
        ):
            with self.subTest(text=text):
                lcov = write_lcov(text)
                try:
                    hits, error = gate.read_lcov(lcov)
                finally:
                    Path(lcov).unlink()
                self.assertIsNone(hits)
                self.assertIn("DA", error)

    def test_malformed_lcov_records_with_valid_data_are_blindness(self):
        for record in ("DA:bad,1", "DA:1", "DA:0,0", "DA:-1,1", "DA:1,-1"):
            text = f"SF:src/lib.rs\nDA:1,1\n{record}\nend_of_record\n"
            with self.subTest(record=record):
                lcov = write_lcov(text)
                try:
                    hits, error = gate.read_lcov(lcov)
                finally:
                    Path(lcov).unlink()
                self.assertIsNone(hits)
                self.assertIn("DA", error)

    def test_unterminated_section_at_eof_is_blindness(self):
        for text in (
            "SF:src/lib.rs\nDA:1,1\n",
            "SF:src/lib.rs\nDA:1,1\nend_of_record\nSF:src/other.rs\nDA:2,1\n",
        ):
            with self.subTest(text=text):
                lcov = write_lcov(text)
                try:
                    hits, error = gate.read_lcov(lcov)
                finally:
                    Path(lcov).unlink()
                self.assertIsNone(hits)
                self.assertIn("DA", error)

    def test_new_section_before_end_of_record_is_blindness(self):
        text = "SF:src/lib.rs\nDA:1,1\nSF:src/other.rs\nDA:2,1\nend_of_record\n"
        lcov = write_lcov(text)
        try:
            hits, error = gate.read_lcov(lcov)
        finally:
            Path(lcov).unlink()
        self.assertIsNone(hits)
        self.assertIn("DA", error)

    def test_empty_source_with_data_after_valid_records_is_blindness(self):
        text = "SF:src/lib.rs\nDA:1,1\nend_of_record\nSF:\nDA:2,1\nend_of_record\n"
        lcov = write_lcov(text)
        try:
            hits, error = gate.read_lcov(lcov)
        finally:
            Path(lcov).unlink()
        self.assertIsNone(hits)
        self.assertIn("DA", error)


class EvaluateTests(unittest.TestCase):
    def test_only_changed_executable_lines_count(self):
        hits = {("a.rs", 1): 1, ("a.rs", 2): 0, ("a.rs", 3): 9}
        covered, coverable, uncovered = gate.evaluate(
            hits, {("a.rs", 1), ("a.rs", 2), ("a.rs", 4)})
        self.assertEqual((covered, coverable), (1, 2))
        self.assertEqual(uncovered, [("a.rs", 2)])

    def test_absolute_lcov_paths_resolve_against_the_root(self):
        hits = {(str(gate.ROOT / "crates/x.rs"), 7): 1}
        covered, coverable, _ = gate.evaluate(hits, {("crates/x.rs", 7)})
        self.assertEqual((covered, coverable), (1, 1))

    def test_changed_source_file_without_lcov_record_is_uncovered(self):
        hits = {("crates/bot/src/voice_rooms.rs", 1): 1}
        covered, coverable, uncovered = gate.evaluate(
            hits, {("crates/bot/src/voice_rooms.rs", 1),
                   ("crates/bot/src/gateway.rs", 300)})
        self.assertEqual((covered, coverable), (1, 2))
        self.assertEqual(uncovered, [("crates/bot/src/gateway.rs", 300)])

    def test_changed_functionless_source_without_lcov_record_stays_unmeasured(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source = root / "crates/core/src/lib.rs"
            source.parent.mkdir(parents=True)
            source.write_text("/// module docs\\n#[cfg(feature = \\\"db\\\")]\\npub mod store;\\n")
            real_root = gate.ROOT
            gate.ROOT = root
            try:
                result = gate.evaluate({}, {("crates/core/src/lib.rs", 1),
                                            ("crates/core/src/lib.rs", 2),
                                            ("crates/core/src/lib.rs", 3)})
            finally:
                gate.ROOT = real_root
        self.assertEqual(result, (0, 0, []))

    def test_trait_declaration_without_a_body_stays_unmeasured(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source = root / "crates/core/src/my_trait.rs"
            source.parent.mkdir(parents=True)
            source.write_text("pub trait Foo {\n    fn bar(&self);\n}\n")
            real_root = gate.ROOT
            gate.ROOT = root
            try:
                result = gate.evaluate({}, {("crates/core/src/my_trait.rs", 2)})
            finally:
                gate.ROOT = real_root
        self.assertEqual(result, (0, 0, []))

    def test_extern_c_function_without_lcov_record_is_uncovered(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source = root / "crates/core/src/ffi.rs"
            source.parent.mkdir(parents=True)
            source.write_text('pub extern "C" fn handler() {}\n')
            real_root = gate.ROOT
            gate.ROOT = root
            try:
                result = gate.evaluate({}, {("crates/core/src/ffi.rs", 1)})
            finally:
                gate.ROOT = real_root
        self.assertEqual(result, (0, 1, [("crates/core/src/ffi.rs", 1)]))

    def test_array_semicolon_in_function_signature_is_instrumentable(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source = root / "crates/core/src/digest.rs"
            source.parent.mkdir(parents=True)
            source.write_text("pub fn digest(input: [u8; 32]) { let _ = input; }\\n")
            real_root = gate.ROOT
            gate.ROOT = root
            try:
                result = gate.evaluate({}, {("crates/core/src/digest.rs", 1)})
            finally:
                gate.ROOT = real_root
        self.assertEqual(result, (0, 1, [("crates/core/src/digest.rs", 1)]))

    def test_array_semicolon_in_return_type_is_instrumentable(self):
        # The bracket guard is only observable on the return-type shape: the
        # `;` sits at paren depth zero, so only the bracket depth keeps it
        # from ending the signature before the body brace.
        self.assertTrue(gate._contains_fn_body("pub fn digest() -> [u8; 32] { [0; 32] }\n"))
        self.assertFalse(gate._contains_fn_body("trait T {\n    fn digest() -> [u8; 32];\n}\n"))

    def test_semicolon_char_literal_in_const_generic_does_not_end_signature(self):
        self.assertTrue(gate._contains_fn_body("fn f<const C: char = ';'>() { }\n"))

    def test_unbalanced_block_comment_opener_before_a_fn_does_not_hide_it(self):
        # A glob such as `src/internal/*` in a line comment or a string has an
        # unbalanced `/*`; it must never make a later real fn invisible.
        cases = {
            "line-comment glob": "//! Modules under src/internal/* are private.\npub fn body_hash() { }\n",
            "doc-comment glob": "/// matches src/*\npub fn run() { }\n",
            "string glob": 'const GLOB: &str = "src/*";\npub fn run() { }\n',
            "inline comment before fn": "const X: u8 = 1; // see src/*\npub fn run() { }\n",
        }
        for label, source in cases.items():
            with self.subTest(label=label):
                self.assertTrue(gate._contains_fn_body(source))

    def test_unbalanced_block_comment_opener_keeps_changed_file_uncovered(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source = root / "crates/core/src/internal_actions.rs"
            source.parent.mkdir(parents=True)
            source.write_text("//! Actions live under src/internal/*.\n\npub fn body_hash() { }\n")
            real_root = gate.ROOT
            gate.ROOT = root
            try:
                result = gate.evaluate({}, {("crates/core/src/internal_actions.rs", 3)})
            finally:
                gate.ROOT = real_root
        self.assertEqual(result, (0, 1, [("crates/core/src/internal_actions.rs", 3)]))

    def test_measurable_source_exclusions_and_root_source_are_pinned(self):
        cases = {
            "crates/bot/src/gateway.rs": True,
            "src/lib.rs": True,
            "crates/bot/src/tests.rs": False,
            "crates/bot/src/test_support.rs": False,
            "crates/bot/src/foo_test.rs": False,
            "crates/bot/src/foo_tests.rs": False,
            "crates/bot/src/tests/mod.rs": False,
            "crates/bot/tests/integration.rs": False,
            "docs/notes.rs": False,
            "docs/notes.md": False,
        }
        for path, expected in cases.items():
            with self.subTest(path=path):
                self.assertEqual(gate.is_measurable_source(path), expected)

    def test_uninstrumented_test_files_and_non_sources_stay_unmeasured(self):
        hits = {("crates/bot/src/voice_rooms.rs", 1): 1}
        for path in ("crates/bot/src/audit_gateway_tests.rs",
                     "crates/bot/tests/integration.rs",
                     "docs/notes.md"):
            with self.subTest(path=path):
                covered, coverable, uncovered = gate.evaluate(
                    hits, {(path, 1)})
                self.assertEqual((covered, coverable), (0, 0))
                self.assertEqual(uncovered, [])


class EndToEndTests(unittest.TestCase):
    def run_gate(self, lcov_text, added):
        lcov = write_lcov(lcov_text)
        real_read, real_added = gate.read_lcov, gate.added_lines
        gate.read_lcov = lambda _path: real_read(lcov)
        gate.added_lines = lambda _base, _head: (set(added), "")
        out = io.StringIO()
        try:
            with redirect_stdout(out):
                code = gate.main(["--lcov", lcov, "--threshold", "80"])
        finally:
            gate.read_lcov, gate.added_lines = real_read, real_added
            Path(lcov).unlink()
        return code, out.getvalue()

    def test_below_threshold_fails_hard(self):
        code, output = self.run_gate(LCOV, {("crates/bot/src/gateway_commands.rs", 1),
                                            ("crates/bot/src/gateway_commands.rs", 2)})
        self.assertEqual(code, 1)
        self.assertIn("::error::", output)
        self.assertIn("50.0%", output)

    def test_at_threshold_is_silent(self):
        code, output = self.run_gate(LCOV, {("crates/bot/src/gateway_commands.rs", 1),
                                            ("crates/bot/src/gateway_commands.rs", 3)})
        self.assertEqual(code, 0)
        self.assertNotIn("::error::", output)
        self.assertNotIn("::warning::", output)

    def test_no_coverable_lines_passes_with_a_note(self):
        code, output = self.run_gate(LCOV, {("other.rs", 1)})
        self.assertEqual(code, 0)
        self.assertNotIn("::error::", output)
        self.assertIn("no coverable changed lines", output)

    def test_uninstrumented_bin_change_fails_instead_of_no_coverable_note(self):
        lib_lcov = ("TN:\nSF:crates/bot/src/voice_rooms.rs\nDA:1,1\n"
                    "end_of_record\n")
        code, output = self.run_gate(lib_lcov, {("crates/bot/src/gateway.rs", 300)})
        self.assertEqual(code, 1)
        self.assertIn("::error::", output)
        self.assertNotIn("no coverable changed lines", output)
        self.assertIn("crates/bot/src/gateway.rs:300", output)

    def test_missing_lcov_fails_closed(self):
        out = io.StringIO()
        with redirect_stdout(out):
            code = gate.main(["--lcov", "/nonexistent/lcov.info"])
        self.assertEqual(code, 1)
        self.assertIn("::error::", out.getvalue())
        self.assertIn("blind", out.getvalue())

    def test_empty_lcov_fails_closed(self):
        lcov = write_lcov("")
        out = io.StringIO()
        try:
            with redirect_stdout(out):
                code = gate.main(["--lcov", lcov])
        finally:
            Path(lcov).unlink()
        self.assertEqual(code, 1)
        self.assertIn("::error::", out.getvalue())
        self.assertIn("blind", out.getvalue())

    def test_unreadable_diff_fails_closed(self):
        lcov = write_lcov(LCOV)
        real_read, real_added = gate.read_lcov, gate.added_lines
        gate.read_lcov = lambda _path: real_read(lcov)
        gate.added_lines = lambda _base, _head: (None, "diff unavailable")
        out = io.StringIO()
        try:
            with redirect_stdout(out):
                code = gate.main(["--lcov", lcov])
        finally:
            gate.read_lcov, gate.added_lines = real_read, real_added
            Path(lcov).unlink()
        self.assertEqual(code, 1)
        self.assertIn("::error::", out.getvalue())
        self.assertIn("could not read the diff", out.getvalue())


class DiffTests(unittest.TestCase):
    def test_added_lines_tracks_new_file_lines(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            subprocess.run(["git", "init", "-q"], cwd=root, check=True)
            subprocess.run(["git", "config", "user.email", "t@t"], cwd=root, check=True)
            subprocess.run(["git", "config", "user.name", "t"], cwd=root, check=True)
            (root / "a.rs").write_text("one\n")
            subprocess.run(["git", "add", "-A"], cwd=root, check=True)
            subprocess.run(["git", "commit", "-qm", "x"], cwd=root, check=True)
            base = subprocess.run(["git", "rev-parse", "HEAD"], cwd=root,
                                  capture_output=True, text=True, check=True).stdout.strip()
            (root / "a.rs").write_text("one\ntwo\n")
            subprocess.run(["git", "add", "-A"], cwd=root, check=True)
            subprocess.run(["git", "commit", "-qm", "y"], cwd=root, check=True)
            head = subprocess.run(["git", "rev-parse", "HEAD"], cwd=root,
                                  capture_output=True, text=True, check=True).stdout.strip()
            real_root = gate.ROOT
            gate.ROOT = root
            try:
                added, error = gate.added_lines(base, head)
            finally:
                gate.ROOT = real_root
        self.assertEqual(error, "")
        self.assertEqual(added, {("a.rs", 2)})


if __name__ == "__main__":
    unittest.main()
