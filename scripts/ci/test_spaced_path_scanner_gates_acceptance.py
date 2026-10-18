"""Acceptance tests: scanner gates must match findings in spaced filenames.

Git terminates the ``+++ b/<path>`` diff header with a TAB delimiter when the
path contains spaces. Both HARD gates (Semgrep and unwrap request-paths) must
strip only that delimiter so the diff-derived ``(path, line)`` set intersects
the scanner's exact repository-relative filename. A fresh enforced finding on
an added line of a spaced filename must therefore fail the gate, while clean
added lines, unchanged findings, renames, non-ASCII names, no-newline markers,
multiline findings and advisory-rule findings keep their existing
dispositions. Unsupported quoted or malformed headers must still fail closed.

Everything is offline: throwaway Git repositories produce the real
TAB-delimited headers and fake scanner binaries emit synthetic reports. No
scanner download, Rust build, rule, threshold, exclusion or CI-lane change is
involved.
"""

import importlib.util
import io
import json
import os
import shlex
import stat
import subprocess
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch


def load_gate(name, filename):
    spec = importlib.util.spec_from_file_location(
        name, Path(__file__).with_name(filename))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


semgrep_gate = load_gate("semgrep_gate_acceptance", "check-semgrep-warn.py")
unwrap_gate = load_gate(
    "unwrap_gate_acceptance", "check-unwrap-request-paths.py")

SPACED = "crates/core/src/command handler.rs"
DOUBLE_SPACED = "crates/core/src/command  handler.rs"
NONASCII_SPACED = "crates/core/src/café handler.rs"
UNWRAP_SPACED = "crates/bot/src/command handler.rs"


def scratch_dir():
    for var in ("PAPERCLIP_RUN_SCRATCH_DIR", "PAPERCLIP_SCRATCH_DIR"):
        value = os.environ.get(var)
        if value and Path(value).is_dir():
            return value
    return None


def make_repo():
    tmp = tempfile.TemporaryDirectory(dir=scratch_dir())
    root = Path(tmp.name)
    subprocess.run(["git", "init", "-q"], cwd=root, check=True)
    subprocess.run(["git", "config", "user.email", "t@t"], cwd=root, check=True)
    subprocess.run(["git", "config", "user.name", "t"], cwd=root, check=True)
    return tmp, root


def commit(root, files):
    for path, content in files.items():
        full = root / path
        full.parent.mkdir(parents=True, exist_ok=True)
        full.write_text(content)
    subprocess.run(["git", "add", "-A"], cwd=root, check=True)
    subprocess.run(["git", "commit", "-qm", "x"], cwd=root, check=True)
    return subprocess.run(
        ["git", "rev-parse", "HEAD"], cwd=root,
        capture_output=True, text=True, check=True).stdout.strip()


def rename_commit(root, src, dst):
    (root / src).rename(root / dst)
    subprocess.run(["git", "add", "-A"], cwd=root, check=True)
    subprocess.run(["git", "commit", "-qm", "rename"], cwd=root, check=True)
    return subprocess.run(
        ["git", "rev-parse", "HEAD"], cwd=root,
        capture_output=True, text=True, check=True).stdout.strip()


def git_diff(root, base, head):
    return subprocess.run(
        ["git", "diff", base, head], cwd=root,
        capture_output=True, text=True, check=True).stdout


def fake_scanner(payload, name):
    tmp = tempfile.TemporaryDirectory(dir=scratch_dir())
    fixture = Path(tmp.name) / "report.json"
    fixture.write_text(payload)
    binary = Path(tmp.name) / name
    binary.write_text("#!/bin/sh\ncat %s\n" % shlex.quote(str(fixture)))
    binary.chmod(binary.stat().st_mode | stat.S_IXUSR)
    return tmp, str(binary)


def run_added(gate, root, base, head):
    real_root, gate.ROOT = gate.ROOT, root
    try:
        return gate.added_lines(base, head)
    finally:
        gate.ROOT = real_root


def run_main(gate, root, argv):
    real_root, gate.ROOT = gate.ROOT, root
    out = io.StringIO()
    try:
        with redirect_stdout(out):
            code = gate.main(argv)
    finally:
        gate.ROOT = real_root
    return code, out.getvalue()


class GitHeaderEvidenceTests(unittest.TestCase):
    def test_git_generates_tab_delimited_header_for_spaced_path(self):
        tmp, root = make_repo()
        try:
            base = commit(root, {SPACED: "fn a() {}\n"})
            head = commit(root, {SPACED: 'fn a() {}\npanic!("new");\n'})
            diff = git_diff(root, base, head)
        finally:
            tmp.cleanup()
        self.assertIn("+++ b/%s\t\n" % SPACED, diff)


class SemgrepSpacedPathTests(unittest.TestCase):
    def run_semgrep_gate(self, root, base, head, report):
        tmp, binary = fake_scanner(json.dumps(report), "semgrep")
        try:
            return run_main(semgrep_gate, root, [
                "--base-ref", base, "--head-ref", head, "--semgrep", binary])
        finally:
            tmp.cleanup()

    def test_added_lines_returns_exact_spaced_path_without_tab(self):
        tmp, root = make_repo()
        try:
            base = commit(root, {SPACED: "fn a() {}\n"})
            head = commit(root, {SPACED: 'fn a() {}\npanic!("new");\n'})
            added, error = run_added(semgrep_gate, root, base, head)
        finally:
            tmp.cleanup()
        self.assertEqual(error, "")
        self.assertEqual(added, {(SPACED, 2)})

    def test_added_lines_preserves_double_spaced_and_nonascii_spaced_paths(self):
        for path in (DOUBLE_SPACED, NONASCII_SPACED):
            with self.subTest(path=path):
                tmp, root = make_repo()
                try:
                    base = commit(root, {path: "fn a() {}\n"})
                    head = commit(root, {path: 'fn a() {}\npanic!("new");\n'})
                    added, error = run_added(semgrep_gate, root, base, head)
                finally:
                    tmp.cleanup()
                self.assertEqual(error, "")
                self.assertEqual(added, {(path, 2)})

    def test_added_lines_ignores_no_newline_marker_in_spaced_path(self):
        tmp, root = make_repo()
        try:
            base = commit(root, {SPACED: "fn a() {}"})
            head = commit(root, {SPACED: 'fn a() { panic!("new"); }'})
            added, error = run_added(semgrep_gate, root, base, head)
        finally:
            tmp.cleanup()
        self.assertEqual(error, "")
        self.assertEqual(added, {(SPACED, 1)})

    def test_main_fails_on_fresh_enforced_finding_in_spaced_path(self):
        tmp, root = make_repo()
        try:
            base = commit(root, {SPACED: "fn a() {}\n"})
            head = commit(root, {SPACED: 'fn a() {}\npanic!("new");\n'})
            self.assertIn("+++ b/%s\t\n" % SPACED, git_diff(root, base, head))
            report = {"results": [{
                "check_id": "rust-warn.rust-panic-in-prod",
                "path": SPACED, "start": {"line": 2}}]}
            code, output = self.run_semgrep_gate(root, base, head, report)
        finally:
            tmp.cleanup()
        self.assertEqual(code, 1)
        self.assertIn("::error::", output)
        self.assertIn("command handler.rs:2", output)

    def test_main_fails_on_multiline_empty_expect_tail_in_spaced_path(self):
        tmp, root = make_repo()
        try:
            base = commit(root, {SPACED:
                                 "let v = Some(1u8)\n    .map(|x| x + 1);\n"})
            head = commit(root, {SPACED:
                                 "let v = Some(1u8)\n    .map(|x| x + 1)\n"
                                 '    .expect("");\n'})
            report = {"results": [{
                "check_id": "rust-warn.rust-empty-expect",
                "path": SPACED, "start": {"line": 1}, "end": {"line": 3}}]}
            code, output = self.run_semgrep_gate(root, base, head, report)
        finally:
            tmp.cleanup()
        self.assertEqual(code, 1)
        self.assertIn("command handler.rs:3", output)

    def test_main_passes_when_spaced_path_added_lines_are_clean(self):
        tmp, root = make_repo()
        try:
            base = commit(root, {SPACED: "fn a() {}\n"})
            head = commit(root, {SPACED: "fn a() {}\nfn b() {}\n"})
            code, output = self.run_semgrep_gate(
                root, base, head, {"results": []})
        finally:
            tmp.cleanup()
        self.assertEqual(code, 0)
        self.assertNotIn("::error::", output)

    def test_main_passes_for_unchanged_finding_in_spaced_path(self):
        tmp, root = make_repo()
        try:
            base = commit(root, {SPACED: 'panic!("old");\nfn a() {}\n'})
            head = commit(
                root, {SPACED: 'panic!("old");\nfn a() {}\nfn b() {}\n'})
            report = {"results": [{
                "check_id": "rust-warn.rust-panic-in-prod",
                "path": SPACED, "start": {"line": 1}}]}
            code, output = self.run_semgrep_gate(root, base, head, report)
        finally:
            tmp.cleanup()
        self.assertEqual(code, 0)
        self.assertNotIn("::error::", output)

    def test_main_does_not_match_scanner_path_truncated_at_first_space(self):
        # Stripping the TAB must not become splitting at the first space:
        # a scanner path that is only the first token is a different file.
        tmp, root = make_repo()
        try:
            base = commit(root, {SPACED: "fn a() {}\n"})
            head = commit(root, {SPACED: 'fn a() {}\npanic!("new");\n'})
            report = {"results": [{
                "check_id": "rust-warn.rust-panic-in-prod",
                "path": SPACED.split(" ")[0], "start": {"line": 2}}]}
            code, output = self.run_semgrep_gate(root, base, head, report)
        finally:
            tmp.cleanup()
        self.assertEqual(code, 0)
        self.assertNotIn("::error::", output)

    def test_pure_rename_to_spaced_path_adds_no_lines_and_passes(self):
        tmp, root = make_repo()
        try:
            base = commit(
                root, {"crates/core/src/handler.rs": 'panic!("old");\ntwo\n'})
            head = rename_commit(
                root, "crates/core/src/handler.rs", SPACED)
            added, error = run_added(semgrep_gate, root, base, head)
            report = {"results": [{
                "check_id": "rust-warn.rust-panic-in-prod",
                "path": SPACED, "start": {"line": 1}}]}
            code, output = self.run_semgrep_gate(root, base, head, report)
        finally:
            tmp.cleanup()
        self.assertEqual(error, "")
        self.assertEqual(added, set())
        self.assertEqual(code, 0)
        self.assertNotIn("::error::", output)

    def test_quoted_or_malformed_header_still_fails_closed(self):
        headers = (
            '+++ "b/crates/core/src/command handler.rs"',
            "+++ crates/core/src/command handler.rs",
        )
        for header in headers:
            with self.subTest(header=header):
                diff = "\n".join((
                    "diff --git a/x b/x", "--- a/x", header,
                    "@@ -0,0 +1 @@", '+panic!("new");', ""))
                with patch.object(
                        semgrep_gate.subprocess, "run",
                        return_value=SimpleNamespace(
                            returncode=0, stdout=diff, stderr="")):
                    added, error = semgrep_gate.added_lines("base", "head")
                self.assertIsNone(added)
                self.assertTrue(error)


class UnwrapSpacedPathTests(unittest.TestCase):
    def run_unwrap_gate(self, root, base, head, scan):
        tmp, binary = fake_scanner(json.dumps(scan), "ast-grep")
        try:
            return run_main(unwrap_gate, root, [
                "--base-ref", base, "--head-ref", head, "--ast-grep", binary])
        finally:
            tmp.cleanup()

    @staticmethod
    def finding(rule_id, path, one_based_line):
        # ast-grep reports 0-based lines; the gate keys hits 1-based.
        zero = one_based_line - 1
        return {"ruleId": rule_id, "file": path,
                "range": {"start": {"line": zero}, "end": {"line": zero}}}

    def test_added_lines_returns_exact_spaced_path_without_tab(self):
        tmp, root = make_repo()
        try:
            base = commit(root, {UNWRAP_SPACED: "fn a() {}\n"})
            head = commit(
                root, {UNWRAP_SPACED: "fn a() {}\nb.unwrap();\n"})
            added, error = run_added(unwrap_gate, root, base, head)
        finally:
            tmp.cleanup()
        self.assertEqual(error, "")
        self.assertEqual(added, {(UNWRAP_SPACED, 2)})

    def test_added_lines_preserves_nonascii_spaced_path(self):
        path = "crates/bot/src/café handler.rs"
        tmp, root = make_repo()
        try:
            base = commit(root, {path: "fn a() {}\n"})
            head = commit(root, {path: "fn a() {}\nb.unwrap();\n"})
            added, error = run_added(unwrap_gate, root, base, head)
        finally:
            tmp.cleanup()
        self.assertEqual(error, "")
        self.assertEqual(added, {(path, 2)})

    def test_main_fails_on_fresh_unwrap_in_spaced_path(self):
        tmp, root = make_repo()
        try:
            base = commit(root, {UNWRAP_SPACED: "fn a() {}\n"})
            head = commit(
                root, {UNWRAP_SPACED: "fn a() {}\nb.unwrap();\n"})
            self.assertIn(
                "+++ b/%s\t\n" % UNWRAP_SPACED, git_diff(root, base, head))
            scan = [self.finding(unwrap_gate.RULE_ID, UNWRAP_SPACED, 2)]
            code, output = self.run_unwrap_gate(root, base, head, scan)
        finally:
            tmp.cleanup()
        self.assertEqual(code, 1)
        self.assertIn("::error::", output)
        self.assertIn("command handler.rs:2", output)

    def test_main_fails_on_fresh_macro_unwrap_in_spaced_path(self):
        tmp, root = make_repo()
        try:
            base = commit(root, {UNWRAP_SPACED: "fn a() {}\n"})
            head = commit(root, {UNWRAP_SPACED:
                                 "fn a() {}\nassert_eq!(x.unwrap(), 1);\n"})
            scan = [self.finding(
                unwrap_gate.MACRO_RULE_ID, UNWRAP_SPACED, 2)]
            code, output = self.run_unwrap_gate(root, base, head, scan)
        finally:
            tmp.cleanup()
        self.assertEqual(code, 1)
        self.assertIn("::error::", output)
        self.assertIn("command handler.rs:2", output)

    def test_main_fails_on_multiline_unwrap_tail_in_spaced_path(self):
        tmp, root = make_repo()
        try:
            base = commit(root, {UNWRAP_SPACED:
                                 "let v = Some(1u8)\n    .map(|x| x + 1);\n"})
            head = commit(root, {UNWRAP_SPACED:
                                 "let v = Some(1u8)\n    .map(|x| x + 1)\n"
                                 "    .unwrap();\n"})
            scan = [{"ruleId": unwrap_gate.RULE_ID, "file": UNWRAP_SPACED,
                     "range": {"start": {"line": 0}, "end": {"line": 2}}}]
            code, output = self.run_unwrap_gate(root, base, head, scan)
        finally:
            tmp.cleanup()
        self.assertEqual(code, 1)
        self.assertIn("command handler.rs:3", output)

    def test_main_passes_when_spaced_path_added_lines_are_clean(self):
        tmp, root = make_repo()
        try:
            base = commit(root, {UNWRAP_SPACED: "fn a() {}\n"})
            head = commit(root, {UNWRAP_SPACED: "fn a() {}\nfn b() {}\n"})
            code, output = self.run_unwrap_gate(root, base, head, [])
        finally:
            tmp.cleanup()
        self.assertEqual(code, 0)
        self.assertNotIn("::error::", output)

    def test_main_passes_for_unchanged_unwrap_in_spaced_path(self):
        tmp, root = make_repo()
        try:
            base = commit(
                root, {UNWRAP_SPACED: "b.unwrap();\nfn a() {}\n"})
            head = commit(
                root, {UNWRAP_SPACED: "b.unwrap();\nfn a() {}\nfn b() {}\n"})
            scan = [self.finding(unwrap_gate.RULE_ID, UNWRAP_SPACED, 1)]
            code, output = self.run_unwrap_gate(root, base, head, scan)
        finally:
            tmp.cleanup()
        self.assertEqual(code, 0)
        self.assertNotIn("::error::", output)

    def test_main_advisory_finding_in_spaced_path_warns_but_passes(self):
        tmp, root = make_repo()
        try:
            base = commit(root, {UNWRAP_SPACED: "fn a() {}\n"})
            head = commit(
                root, {UNWRAP_SPACED: "fn a() {}\nfn b() {}\n"})
            scan = [self.finding("rust-erased-map-err", UNWRAP_SPACED, 2)]
            code, output = self.run_unwrap_gate(root, base, head, scan)
        finally:
            tmp.cleanup()
        self.assertEqual(code, 0)
        self.assertNotIn("::error::", output)
        self.assertIn("::warning::", output)
        self.assertIn("rust-erased-map-err", output)

    def test_main_does_not_match_scanner_path_truncated_at_first_space(self):
        tmp, root = make_repo()
        try:
            base = commit(root, {UNWRAP_SPACED: "fn a() {}\n"})
            head = commit(
                root, {UNWRAP_SPACED: "fn a() {}\nb.unwrap();\n"})
            scan = [self.finding(
                unwrap_gate.RULE_ID, UNWRAP_SPACED.split(" ")[0], 2)]
            code, output = self.run_unwrap_gate(root, base, head, scan)
        finally:
            tmp.cleanup()
        self.assertEqual(code, 0)
        self.assertNotIn("::error::", output)

    def test_pure_rename_to_spaced_path_adds_no_lines_and_passes(self):
        tmp, root = make_repo()
        try:
            base = commit(
                root, {"crates/bot/src/handler.rs": "b.unwrap();\ntwo\n"})
            head = rename_commit(
                root, "crates/bot/src/handler.rs", UNWRAP_SPACED)
            added, error = run_added(unwrap_gate, root, base, head)
            scan = [self.finding(unwrap_gate.RULE_ID, UNWRAP_SPACED, 1)]
            code, output = self.run_unwrap_gate(root, base, head, scan)
        finally:
            tmp.cleanup()
        self.assertEqual(error, "")
        self.assertEqual(added, set())
        self.assertEqual(code, 0)
        self.assertNotIn("::error::", output)

    def test_quoted_or_malformed_header_still_fails_closed(self):
        headers = (
            '+++ "b/crates/bot/src/command handler.rs"',
            "+++ crates/bot/src/command handler.rs",
        )
        for header in headers:
            with self.subTest(header=header):
                diff = "\n".join((
                    "diff --git a/x b/x", "--- a/x", header,
                    "@@ -0,0 +1 @@", "+b.unwrap();", ""))
                with patch.object(
                        unwrap_gate.subprocess, "run",
                        return_value=SimpleNamespace(
                            returncode=0, stdout=diff, stderr="")):
                    added, error = unwrap_gate.added_lines("base", "head")
                self.assertIsNone(added)
                self.assertTrue(error)


if __name__ == "__main__":
    unittest.main()
